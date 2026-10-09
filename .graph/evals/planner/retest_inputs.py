import argparse, copy, json, re
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import yaml

import compare


def tool_of(step):
    return step.get('tool_name') or step.get('toolName')


def with_inputs(doc, given):
    doc = copy.deepcopy(doc)
    properties, required, renames = {}, [], {}
    leftover = list(given.items())
    kept = []
    for step in doc.get('steps') or []:
        if tool_of(step) != 'ask':
            kept.append(step)
            continue
        schema = (step.get('input') or {}).get('output_schema') or (step.get('input') or {}).get('outputSchema') or {}
        for field, spec in (schema.get('properties') or {}).items():
            properties[field] = spec
            renames[(step['id'], field)] = field
    for field in properties:
        if field in given:
            required.append(field)
            leftover = [(k, v) for k, v in leftover if k != field]
    for field in properties:
        if field not in required and leftover:
            required.append(field)
            leftover.pop(0)
    text = json.dumps(kept)
    for (ask_id, field), name in renames.items():
        text = re.sub(r'\{\{\s*' + re.escape(ask_id) + r'\.answer\.' + re.escape(field) + r'\s*\}\}', '{{input.' + name + '}}', text)
    doc['steps'] = json.loads(text)
    for name in sorted(set(re.findall(r'\{\{\s*input\.(\w+)', text))):
        if name not in properties:
            value = given.get(name)
            kind = {bool: 'boolean', int: 'integer', float: 'number', list: 'array', dict: 'object'}.get(type(value), 'string')
            properties[name] = {'type': kind, 'description': name}
            if name in given:
                required.append(name)
                leftover = [(k, v) for k, v in leftover if k != name]
    for name in properties:
        if name not in required and leftover:
            required.append(name)
            leftover.pop(0)
    if properties:
        doc['input_schema'] = {'type': 'object', 'required': required, 'properties': properties}
    if doc.get('finish', 'silent') == 'silent':
        last = next((step for step in reversed(doc['steps']) if tool_of(step) != 'exit'), None)
        if last:
            doc['finish'] = {'output': {'result': '{{' + last['id'] + '}}'}}
    return doc


def retest(record, goal):
    record = dict(record, planner=record['planner'] + '_with_inputs')
    doc = with_inputs(record['plan'], goal['inputs'])
    doc['identifier'] = f"cand_{record['planner']}_{goal['id']}_{record['rep']}"
    (compare.SCRATCH / 'plans' / f"{doc['identifier']}.yaml").write_text(yaml.safe_dump(doc, sort_keys=False, allow_unicode=True))
    record['plan'] = doc
    record['structure'] = compare.structure(doc)
    return compare.execute(record, goal)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('results')
    parser.add_argument('--planner', default='compose_plan')
    parser.add_argument('--workers', type=int, default=4)
    parser.add_argument('--out', default=str(compare.SCRATCH / 'with_inputs.json'))
    parser.add_argument('--root', default=str(compare.ROOT))
    parser.add_argument('--goals-file', default=str(compare.HERE / 'goals.yaml'))
    parser.add_argument('--extra-config')
    args = parser.parse_args()
    compare.use_root(args.root, Path(args.extra_config).read_text() if args.extra_config else '')
    goals = {goal['id']: goal for goal in yaml.safe_load(Path(args.goals_file).read_text())}
    records = [r for r in json.load(open(args.results)) if r['planner'] == args.planner and r.get('plan') and goals[r['goal']].get('reference')]
    try:
        compare.set_plan_paths(include_eval=True)
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            list(pool.map(compare.run_reference, goals.values()))
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            results = list(pool.map(lambda r: retest(r, goals[r['goal']]), records))
    finally:
        compare.restore_config()
    Path(args.out).write_text(json.dumps(results, indent=1, default=str))
    for r in sorted(results, key=lambda r: (r['goal'], r['rep'])):
        print(f"{r['goal']:30} rep {r['rep']} valid {r.get('valid')!s:5} run_exit {r.get('run_exit')!s:4} correct {r.get('correct')}  {(r.get('reason') or '')[:110]}")


if __name__ == '__main__':
    main()
