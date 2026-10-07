import argparse, copy, json
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import yaml

import compare


def edit_steps(value, tool, change):
    if isinstance(value, dict):
        if (value.get('tool_name') or value.get('toolName')) == tool:
            change(value)
        for inner in value.values():
            edit_steps(inner, tool, change)
    elif isinstance(value, list):
        for inner in value:
            edit_steps(inner, tool, change)


def maps_calling(value, tool, change):
    if isinstance(value, dict):
        if (value.get('tool_name') or value.get('toolName')) == 'map' and tool in json.dumps(value.get('input', {}).get('do')):
            change(value)
        for inner in value.values():
            maps_calling(inner, tool, change)
    elif isinstance(value, list):
        for inner in value:
            maps_calling(inner, tool, change)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('results')
    parser.add_argument('--goal', required=True)
    parser.add_argument('--set-input', nargs=3, metavar=('TOOL', 'KEY', 'VALUE'), action='append', default=[])
    parser.add_argument('--skip-on', help='add onError: skip to maps whose body calls this tool')
    parser.add_argument('--tag', required=True)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    goals = {goal['id']: goal for goal in yaml.safe_load((compare.HERE / 'goals.yaml').read_text())}
    records = []
    for record in json.load(open(args.results)):
        if record['goal'] != args.goal or not record.get('plan'):
            continue
        doc = copy.deepcopy(record['plan'])
        for tool, key, value in args.set_input:
            edit_steps(doc['steps'], tool, lambda step: step.setdefault('input', {}).__setitem__(key, value))
        if args.skip_on:
            maps_calling(doc['steps'], args.skip_on, lambda step: step['input'].__setitem__('onError', 'skip'))
        doc['identifier'] = f"{doc['identifier']}_{args.tag}"
        (compare.SCRATCH / 'plans' / f"{doc['identifier']}.yaml").write_text(yaml.safe_dump(doc, sort_keys=False, allow_unicode=True))
        records.append(dict(record, plan=doc))
    try:
        compare.set_plan_paths(include_eval=True)
        compare.run_reference(goals[args.goal])
        with ThreadPoolExecutor(max_workers=5) as pool:
            rerun = list(pool.map(lambda r: compare.execute(dict(r), goals[r['goal']]), records))
    finally:
        compare.restore_config()
    Path(args.out).write_text(json.dumps(rerun, indent=1, default=str))
    print(f"{args.tag}: {sum(bool(r.get('correct')) for r in rerun)}/{len(rerun)} correct, "
          f"{sum(1 for r in rerun if r.get('run_exit') == 0)}/{len(rerun)} ran to completion")


if __name__ == '__main__':
    main()
