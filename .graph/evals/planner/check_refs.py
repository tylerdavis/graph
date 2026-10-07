import argparse, json
from pathlib import Path
from concurrent.futures import ThreadPoolExecutor

import yaml

import compare


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--goals', nargs='*')
    parser.add_argument('--root', default=str(compare.ROOT))
    parser.add_argument('--goals-file', default=str(compare.HERE / 'goals.yaml'))
    parser.add_argument('--extra-config')
    args = parser.parse_args()
    compare.use_root(args.root, Path(args.extra_config).read_text() if args.extra_config else '')
    goals = [goal for goal in yaml.safe_load(Path(args.goals_file).read_text())
             if goal.get('reference') and (not args.goals or goal['id'] in args.goals)]
    try:
        compare.set_plan_paths(include_eval=True)
        with ThreadPoolExecutor(max_workers=4) as pool:
            runs = list(pool.map(lambda goal: compare.graph(['plan', 'run', goal['reference']], goal['inputs'], timeout=600), goals))
    finally:
        compare.restore_config()
    for goal, ran in zip(goals, runs):
        body = ran['body'] or {}
        result = body.get('output') or body.get('exit') or ran['stderr'][-600:]
        print(f"===== {goal['id']} exit {ran['exit_code']} {ran['seconds']}s")
        print(json.dumps(result, ensure_ascii=False)[:900] if not isinstance(result, str) else result)


if __name__ == '__main__':
    main()
