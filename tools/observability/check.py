#!/usr/bin/env python3
"""Keeps deploy/prometheus-alerts.yml and deploy/grafana/dashboards/*.json in step with the
metrics the node actually registers (crates/aurix-metrics/src/lib.rs).

Checks:
  * every `aurix_*` series referenced by a rule or panel is registered (histogram suffixes
    `_bucket` / `_count` / `_sum` are resolved to their base metric);
  * every label used on a registered metric is one of the metric's declared labels;
  * dashboards parse as JSON, carry a unique `uid`, and every panel has a title and at least
    one query;
  * alert rules parse, have a `severity` label and `summary` + `description` annotations.

Prometheus's own `promtool check rules` (expression syntax) runs separately in CI.
Exit status 1 on the first class of problems found.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
METRICS_RS = ROOT / "crates/aurix-metrics/src/lib.rs"
ALERTS = ROOT / "deploy/prometheus-alerts.yml"
DASHBOARDS = sorted((ROOT / "deploy/grafana/dashboards").glob("*.json"))

# Not emitted by aurix-metrics but present on every scrape (target labels / rate suffixes).
IMPLICIT_LABELS = {"instance", "job", "le", "__name__"}


def registered_metrics() -> dict[str, set[str]]:
    """metric name → declared label names (empty set for unlabelled metrics)."""
    src = METRICS_RS.read_text()
    metrics: dict[str, set[str]] = {}
    for m in re.finditer(
        r'register_(?:int_)?(?:counter|gauge|histogram)(?:_vec)?!\(\s*"(aurix_[a-z0-9_]+)"'
        r'\s*,\s*"[^"]*"\s*(?:,\s*&\[([^\]]*)\])?',
        src,
    ):
        labels = set(re.findall(r'"([a-z_]+)"', m.group(2) or ""))
        metrics[m.group(1)] = labels
    if not metrics:
        sys.exit(f"no metrics found in {METRICS_RS}")
    return metrics


SELECTOR = re.compile(r"\b(aurix_[a-z0-9_]+)\s*(\{[^}]*\})?")
LABEL = re.compile(r"([a-zA-Z_][a-zA-Z0-9_]*)\s*(=~|!~|!=|=)")


def check_expr(expr: str, where: str, metrics: dict[str, set[str]], errors: list[str]) -> None:
    for name, selector in SELECTOR.findall(expr):
        base = re.sub(r"_(bucket|count|sum)$", "", name)
        if name in metrics:
            base = name
        elif base not in metrics:
            errors.append(f"{where}: unknown metric {name}")
            continue
        declared = metrics[base] | IMPLICIT_LABELS
        for label, _ in LABEL.findall(selector or ""):
            if label not in declared:
                errors.append(f"{where}: metric {base} has no label {label!r} (has {sorted(metrics[base])})")


def load_yaml(path: Path):
    try:
        import yaml  # type: ignore
    except ImportError:
        sys.exit("PyYAML is required: pip install pyyaml")
    with path.open() as f:
        return yaml.safe_load(f)


def check_alerts(metrics: dict[str, set[str]], errors: list[str]) -> int:
    doc = load_yaml(ALERTS)
    count = 0
    for group in doc.get("groups", []):
        for rule in group.get("rules", []):
            name = rule.get("alert") or rule.get("record") or "<unnamed>"
            where = f"{ALERTS.name}: {group.get('name')}/{name}"
            count += 1
            if "alert" in rule:
                if "severity" not in rule.get("labels", {}):
                    errors.append(f"{where}: missing labels.severity")
                for key in ("summary", "description"):
                    if key not in rule.get("annotations", {}):
                        errors.append(f"{where}: missing annotations.{key}")
            check_expr(str(rule.get("expr", "")), where, metrics, errors)
    return count


def check_dashboards(metrics: dict[str, set[str]], errors: list[str]) -> int:
    uids: dict[str, str] = {}
    panels_total = 0
    for path in DASHBOARDS:
        try:
            dash = json.loads(path.read_text())
        except json.JSONDecodeError as e:
            errors.append(f"{path.name}: invalid JSON: {e}")
            continue
        uid = dash.get("uid")
        if not uid:
            errors.append(f"{path.name}: missing uid")
        elif uid in uids:
            errors.append(f"{path.name}: uid {uid!r} already used by {uids[uid]}")
        else:
            uids[uid] = path.name

        def walk(panels, parent):
            nonlocal panels_total
            for p in panels:
                title = p.get("title") or "<untitled>"
                where = f"{path.name}: {parent}{title}"
                if p.get("type") == "row":
                    walk(p.get("panels", []), f"{title}/")
                    continue
                panels_total += 1
                if not p.get("title"):
                    errors.append(f"{where}: panel without title")
                targets = p.get("targets", [])
                if not targets:
                    errors.append(f"{where}: panel without queries")
                for t in targets:
                    check_expr(str(t.get("expr", "")), where, metrics, errors)

        walk(dash.get("panels", []), "")
    return panels_total


def main() -> int:
    metrics = registered_metrics()
    errors: list[str] = []
    rules = check_alerts(metrics, errors)
    panels = check_dashboards(metrics, errors)
    for e in errors:
        print(f"error: {e}", file=sys.stderr)
    if errors:
        return 1
    print(
        f"ok: {rules} alert rules, {panels} dashboard panels across {len(DASHBOARDS)} dashboards "
        f"reference only the {len(metrics)} registered metrics"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
