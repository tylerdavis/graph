import argparse, json
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import yaml

import compare


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('results')
    parser.add_argument('--goals', nargs='*')
    parser.add_argument('--root', default=str(compare.ROOT))
    parser.add_argument('--goals-file', default=str(compare.HERE / 'goals.yaml'))
    parser.add_argument('--extra-config')
    parser.add_argument('--workers', type=int, default=5)
    parser.add_argument('--out', required=True)
    args = parser.parse_args()
    compare.use_root(args.root, Path(args.extra_config).read_text() if args.extra_config else '')
    goals = {goal['id']: goal for goal in yaml.safe_load(Path(args.goals_file).read_text())}
    records = [r for r in json.load(open(args.results))
               if r.get('plan') and goals.get(r['goal'], {}).get('reference') and (not args.goals or r['goal'] in args.goals)]
    (compare.SCRATCH / 'plans').mkdir(parents=True, exist_ok=True)
    for record in records:
        doc = record['plan']
        (compare.SCRATCH / 'plans' / f"{doc['identifier']}.yaml").write_text(yaml.safe_dump(doc, sort_keys=False, allow_unicode=True))
    try:
        compare.set_plan_paths(include_eval=True)
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            list(pool.map(compare.run_reference, [goals[g] for g in {r['goal'] for r in records}]))
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            rerun = list(pool.map(lambda r: compare.execute(dict(r), goals[r['goal']]), records))
    finally:
        compare.restore_config()
    Path(args.out).write_text(json.dumps(rerun, indent=1, default=str))
    before = {}
    for r in records:
        before.setdefault(r['goal'], []).append(bool(r.get('correct')))
    after = {}
    for r in rerun:
        after.setdefault(r['goal'], []).append(bool(r.get('correct')))
    for goal in sorted(after):
        print(f"{goal:30} before {sum(before[goal])}/{len(before[goal])}  after {sum(after[goal])}/{len(after[goal])}")


if __name__ == '__main__':
    main()
