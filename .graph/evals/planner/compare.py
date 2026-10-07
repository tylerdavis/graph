import argparse, json, re, subprocess, time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import yaml

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
SCRATCH = Path('/tmp/planner-eval')
CONTROL = {'agent', 'ask', 'exit', 'route', 'filter', 'map', 'reduce'}
CONFIG = ROOT / '.graph' / 'config.toml'
WRITES = re.compile(r'save_|create_|delete_|update_|post|write|sync|mark_|share|retire_|restore_|merge|submit|prepare_|(?<!list)_comments?$')
BASE_PLAN_PATHS = ['./.graph/plans', '~/.config/graph/plans']
EVAL_PLAN_PATHS = [str(HERE / 'plans'), str(SCRATCH / 'plans')]
EXTRA_CONFIG = ''


def use_root(root, extra_config=''):
    global ROOT, CONFIG, EXTRA_CONFIG
    ROOT = Path(root).expanduser().resolve()
    CONFIG = ROOT / '.graph' / 'config.toml'
    EXTRA_CONFIG = extra_config


ORIGINAL_CONFIG = ''


def restore_config():
    if ORIGINAL_CONFIG:
        CONFIG.write_text(ORIGINAL_CONFIG)


def set_plan_paths(include_eval):
    global ORIGINAL_CONFIG
    if not ORIGINAL_CONFIG:
        ORIGINAL_CONFIG = CONFIG.read_text()
    paths = BASE_PLAN_PATHS + (EVAL_PLAN_PATHS if include_eval else [])
    line = 'paths = ' + json.dumps(paths)
    text = ORIGINAL_CONFIG + ('\n' + EXTRA_CONFIG if EXTRA_CONFIG else '')
    section = re.search(r'^\[plans\]\n(?:paths = .*\n)?', text, flags=re.M)
    block = '[plans]\n' + line + '\n'
    text = text[:section.start()] + block + text[section.end():] if section else text.rstrip('\n') + '\n\n' + block
    CONFIG.write_text(text)


def graph(args, inputs=None, timeout=600):
    command = ['graph', *args]
    if inputs is not None:
        command.append(json.dumps(inputs))
    started = time.time()
    try:
        run = subprocess.run(command + ['--json'], cwd=ROOT, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return {'exit_code': 'timeout', 'seconds': timeout, 'body': None, 'stderr': ''}
    try:
        body = json.loads(run.stdout)
    except json.JSONDecodeError:
        body = None
    return {'exit_code': run.returncode, 'seconds': round(time.time() - started, 1), 'body': body,
            'stderr': run.stderr[-3000:]}


def walk_steps(steps):
    for step in steps or []:
        yield step
        tool_input = step.get('input') or {}
        for key in ('do', 'then', 'else'):
            body = tool_input.get(key)
            if isinstance(body, dict):
                yield from walk_steps([body])
            elif isinstance(body, list):
                yield from walk_steps(body)
        for case in (tool_input.get('cases') or {}).values():
            yield from walk_steps(case if isinstance(case, list) else [case])


def structure(doc):
    steps = list(walk_steps(doc.get('steps')))
    tool = lambda step: step.get('tool_name') or step.get('toolName')
    agents = [s for s in steps if tool(s) == 'agent']
    return {
        'top_level_steps': len(doc.get('steps') or []),
        'all_steps': len(steps),
        'tools_used': sorted({tool(s) for s in steps}),
        'agent': len(agents),
        'agent_without_tools': sum(1 for s in agents if not (s.get('input') or {}).get('tools')),
        'agent_chat': sum(1 for s in steps if tool(s) == 'agent__chat'),
        'infer': sum(1 for s in steps if tool(s) == 'builtin__infer'),
        'reshape': sum(1 for s in steps if tool(s) == 'builtin__reshape'),
        'ask': sum(1 for s in steps if tool(s) == 'ask'),
        'has_input_schema': bool(doc.get('input_schema')),
        'finish': 'output' if doc.get('output') else ('solver' if doc.get('solver') else 'none'),
    }


def coerce(value, schema):
    kind = (schema or {}).get('type')
    if kind == 'string' and not isinstance(value, str):
        return str(value)
    if kind in ('integer', 'number') and isinstance(value, str) and value.isdigit():
        return int(value)
    return value


def map_inputs(doc, given):
    schema = doc.get('input_schema') or {}
    properties = schema.get('properties') or {}
    names = list(properties.keys())
    if not names:
        return {}
    mapped = {name: given[name] for name in names if name in given}
    leftover = [value for key, value in given.items() if key not in names]
    for name in names:
        if name not in mapped and leftover:
            mapped[name] = leftover.pop(0)
    return {name: coerce(value, properties.get(name)) for name, value in mapped.items()}


REFERENCES = {}


def run_reference(goal):
    if goal.get('reference'):
        ran = graph(['plan', 'run', goal['reference']], goal['inputs'], timeout=300)
        REFERENCES[goal['id']] = (ran['body'] or {}).get('output') or ran['stderr'][-1500:]


def reference_output(goal):
    return REFERENCES.get(goal['id'])


def draft(planner, goal, rep=0):
    record = {'planner': planner, 'goal': goal['id'], 'rep': rep}
    drafted = graph(['plan', 'run', planner], {'goal': goal['goal']})
    usage = (drafted['body'] or {}).get('usage') or {}
    record.update(draft_seconds=drafted['seconds'], draft_exit=drafted['exit_code'],
                  draft_cost=usage.get('cost_usd'), draft_calls=usage.get('calls'),
                  draft_input_tokens=usage.get('input_tokens'), draft_output_tokens=usage.get('output_tokens'),
                  draft_cached_tokens=usage.get('cache_read_input_tokens'))
    output = (drafted['body'] or {}).get('output') or {}
    record['missing'] = output.get('missing') or []
    record['questions'] = output.get('questions') or []
    doc = output.get('plan')
    if not isinstance(doc, dict) or not doc.get('steps'):
        record['plan'] = None
        record['draft_error'] = drafted['stderr'][-800:] if drafted['exit_code'] else output.get('problems')
        record['correct'] = goal.get('reference') is None and bool(record['missing'])
        return record
    identifier = f"cand_{planner}_{goal['id']}_{rep}"
    doc = dict(doc, identifier=identifier, version=doc.get('version', 2))
    path = SCRATCH / 'plans' / f'{identifier}.yaml'
    path.write_text(yaml.safe_dump(doc, sort_keys=False, allow_unicode=True))
    record['plan'] = doc
    record['structure'] = structure(doc)
    return record


ID_KEYS = {'id', 'identifier', 'ticket', 'key', 'sha', 'short_sha', 'path', 'file'}


def collect(value, pattern, width=None):
    found = set(re.findall(pattern, json.dumps(value, ensure_ascii=False)))
    return {item[:width] for item in found} if width else found


def dicts(value):
    if isinstance(value, dict):
        yield value
        for inner in value.values():
            yield from dicts(inner)
    elif isinstance(value, list):
        for inner in value:
            yield from dicts(inner)
    elif isinstance(value, str) and value.strip()[:1] in '[{':
        try:
            yield from dicts(json.loads(value))
        except json.JSONDecodeError:
            pass


def deterministic(check, reference, candidate):
    expected_from = (reference or {}).get(check['reference_field']) if isinstance(reference, dict) else None
    if expected_from is None:
        return None, 'the reference run returned no ' + check['reference_field']
    if check['kind'] == 'set':
        expected = collect(expected_from, check['pattern'], check.get('width'))
        id_values = [value for cell in dicts(candidate) for key, value in cell.items()
                     if key in ID_KEYS and isinstance(value, str)]
        got = collect(id_values, check['pattern'], check.get('width')) if id_values else set()
        if not got:
            got = collect(candidate, check['pattern'], check.get('width'))
        if got == expected:
            return True, f"same {len(expected)} items as the reference"
        return False, f"missing {sorted(expected - got)[:10]}, extra {sorted(got - expected)[:10]}"
    if check['kind'] == 'rows':
        rows = [row for row in dicts(expected_from) if check['key'] in row]
        cells = list(dicts(candidate))
        missing = [
            f"{row[check['key']]}={row[check['value']]}" for row in rows
            if not any({str(v) for v in cell.values()} >= {str(row[check['key']]), str(row[check['value']])} for cell in cells)
        ]
        if not missing:
            return True, f"all {len(rows)} rows match the reference"
        return False, 'rows missing or wrong: ' + ', '.join(missing)
    if check['kind'] == 'review':
        changed = {row['path'] for row in dicts(expected_from) if 'path' in row}
        findings = [cell for cell in dicts(candidate)
                    if ('file' in cell or 'path' in cell) and any(k in cell for k in ('finding', 'description', 'message', 'issue', 'summary', 'title'))]
        counts = [value for cell in dicts(candidate) for key, value in cell.items()
                  if 'blocker' in key.lower() and isinstance(value, int) and not isinstance(value, bool)]
        problems = []
        for finding in findings:
            where = finding.get('file') or finding.get('path')
            if where not in changed:
                problems.append(f"{where} is not a changed file")
            line = finding.get('line', finding.get('line_number', finding.get('lines')))
            if not (isinstance(line, int) or (isinstance(line, str) and line[:1].isdigit())):
                problems.append(f"{where} has no line")
        flagged = sum(1 for finding in findings if any(
            ('block' in key.lower() and value is True) or (key.lower() == 'severity' and str(value).lower() == 'blocker')
            for key, value in finding.items()))
        if not counts:
            problems.append('no blocker count')
        elif counts[0] != flagged:
            problems.append(f"blocker count {counts[0]} but {flagged} findings flagged as blockers")
        if problems:
            return False, '; '.join(problems[:6])
        return True, f"{len(findings)} findings, all in changed files with lines; blocker count {counts[0]} matches"
    return None, 'unknown check kind'


def execute(record, goal):
    if record.get('plan') is None:
        return record
    doc = record['plan']
    identifier = doc['identifier']
    path = SCRATCH / 'plans' / f'{identifier}.yaml'
    check = subprocess.run(['graph', 'plan', 'validate', str(path)], cwd=ROOT, capture_output=True, text=True)
    record['valid'] = check.returncode == 0
    record['validation'] = (check.stdout + check.stderr).strip()[-800:]
    if goal.get('reference') is None:
        record['correct'] = False
        record['reason'] = 'drafted a plan for a goal no tool can serve'
        return record
    writes = [tool for tool in (record.get('structure') or {}).get('tools_used', []) if WRITES.search(tool)]
    if writes:
        record['correct'] = False
        record['reason'] = 'not run: the plan calls tools that write (' + ', '.join(writes) + '); every goal here is read-only'
        return record
    inputs = map_inputs(doc, goal['inputs'])
    record['run_inputs'] = inputs
    ran = graph(['plan', 'run', identifier], inputs, timeout=600)
    record.update(run_seconds=ran['seconds'], run_exit=ran['exit_code'])
    body = ran['body'] or {}
    candidate = body.get('output') or body.get('answer') or body.get('exit') or ran['stderr'][-1500:]
    record['run_output'] = candidate
    if goal.get('check'):
        correct, reason = deterministic(goal['check'], reference_output(goal), candidate)
        if correct is not None:
            record.update(correct=correct, score=1.0 if correct else 0.0, reason=reason, judged_by='check')
            return record
    record['judged_by'] = 'model'
    verdict = graph(['plan', 'run', 'planner_eval_judge'], {
        'goal': goal['goal'], 'rubric': goal['rubric'],
        'reference_output': json.dumps(reference_output(goal))[:30000],
        'candidate_output': json.dumps(candidate)[:30000],
    })
    judged = (verdict['body'] or {}).get('output') or {}
    record.update(correct=bool(judged.get('correct')), score=judged.get('score'), reason=judged.get('reason'))
    return record


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('planners', nargs='+')
    parser.add_argument('--goals', nargs='*')
    parser.add_argument('--workers', type=int, default=3)
    parser.add_argument('--reps', type=int, default=1)
    parser.add_argument('--root', default=str(ROOT))
    parser.add_argument('--goals-file', default=str(HERE / 'goals.yaml'))
    parser.add_argument('--extra-config', help='a TOML file appended to the root config while the run lasts')
    parser.add_argument('--out', default=str(SCRATCH / 'results.json'))
    args = parser.parse_args()
    use_root(args.root, Path(args.extra_config).read_text() if args.extra_config else '')
    (SCRATCH / 'plans').mkdir(parents=True, exist_ok=True)
    goals = yaml.safe_load(Path(args.goals_file).read_text())
    if args.goals:
        goals = [goal for goal in goals if goal['id'] in args.goals]
    jobs = [(planner, goal, rep) for rep in range(args.reps) for planner in args.planners for goal in goals]
    for stale in (SCRATCH / 'plans').glob('cand_*.yaml'):
        stale.unlink()
    try:
        set_plan_paths(include_eval=False)
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            drafted = list(pool.map(lambda job: draft(*job), jobs))
        set_plan_paths(include_eval=True)
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            list(pool.map(run_reference, goals))
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            records = list(pool.map(lambda pair: execute(*pair), zip(drafted, [goal for _, goal, _ in jobs])))
    finally:
        restore_config()
    Path(args.out).write_text(json.dumps(records, indent=1, default=str))
    for r in sorted(records, key=lambda r: (r['planner'], r['goal'], r['rep'])):
        s = r.get('structure') or {}
        print(f"{r['planner']:14} {r['goal']:30} draft {r['draft_seconds']:6.1f}s ${r.get('draft_cost') or 0:.3f} "
              f"valid {r.get('valid')!s:5} steps {s.get('all_steps', '-')!s:>3} agent {s.get('agent', '-')} "
              f"infer {s.get('infer', '-')} reshape {s.get('reshape', '-')} correct {r.get('correct')}")


if __name__ == '__main__':
    main()
