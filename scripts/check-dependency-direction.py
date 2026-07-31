#!/usr/bin/env python3
"""Validate NMP dependency direction from Cargo's resolved package graph."""

from __future__ import annotations

import heapq
import json
import sys
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Set, Tuple


class CheckError(Exception):
    """A malformed policy/graph or an unclassified NMP workspace package."""


@dataclass(frozen=True)
class RoleAssignment:
    role: str
    origin_kind: str
    origin_rule: str


@dataclass(frozen=True)
class FocusedClassification:
    package: str
    role: str
    origin_kind: str
    origin_rule: str


NodeKey = Tuple[str, str]
PathKey = Tuple[NodeKey, ...]


def load_json(path: Path) -> Dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise CheckError("cannot read {}: {}".format(path, error)) from error
    if not isinstance(value, dict):
        raise CheckError("{} must contain one JSON object".format(path))
    return value


def reject_unknown_keys(
    value: Dict[str, Any], allowed: Set[str], description: str
) -> None:
    unknown = set(value) - allowed
    if unknown:
        raise CheckError(
            "{} contains unknown fields: {}".format(
                description, ", ".join(sorted(unknown))
            )
        )


def parse_policy(
    policy: Dict[str, Any],
) -> Tuple[
    Dict[str, Dict[str, Any]],
    Dict[str, str],
    List[Tuple[str, str]],
    Set[str],
    List[FocusedClassification],
]:
    reject_unknown_keys(
        policy,
        {
            "schema_version",
            "description",
            "dependency_kinds",
            "roles",
            "role_rules",
            "focused_classifications",
        },
        "dependency policy",
    )
    if policy.get("schema_version") != 1:
        raise CheckError("dependency policy schema_version must be 1")

    roles = policy.get("roles")
    rules = policy.get("role_rules")
    dependency_kinds = policy.get("dependency_kinds")
    focused_values = policy.get("focused_classifications")
    if not isinstance(roles, dict) or not roles:
        raise CheckError("dependency policy must define roles")
    if not isinstance(rules, dict):
        raise CheckError("dependency policy must define role_rules")
    if not isinstance(dependency_kinds, list):
        raise CheckError("dependency policy must define dependency_kinds")
    if set(dependency_kinds) != {"normal", "build"} or len(dependency_kinds) != 2:
        raise CheckError("dependency_kinds must be exactly normal and build")
    if not isinstance(focused_values, list):
        raise CheckError("dependency policy must define focused_classifications")

    known_roles = set(roles)
    for role, definition in roles.items():
        if not isinstance(role, str) or not role:
            raise CheckError("role names must be non-empty strings")
        if not isinstance(definition, dict):
            raise CheckError("role {!r} must be an object".format(role))
        reject_unknown_keys(
            definition,
            {"description", "may_reach", "stop_at_roles"},
            "role {!r}".format(role),
        )
        allowed = definition.get("may_reach")
        if allowed != "*" and not (
            isinstance(allowed, list)
            and all(isinstance(item, str) for item in allowed)
        ):
            raise CheckError("role {!r} has invalid may_reach".format(role))
        if isinstance(allowed, list):
            unknown = set(allowed) - known_roles
            if unknown:
                raise CheckError(
                    "role {!r} may_reach references unknown roles: {}".format(
                        role, ", ".join(sorted(unknown))
                    )
                )
        stop_at = definition.get("stop_at_roles", [])
        if not (
            isinstance(stop_at, list)
            and all(isinstance(item, str) for item in stop_at)
        ):
            raise CheckError("role {!r} has invalid stop_at_roles".format(role))
        unknown_stops = set(stop_at) - known_roles
        if unknown_stops:
            raise CheckError(
                "role {!r} stop_at_roles references unknown roles: {}".format(
                    role, ", ".join(sorted(unknown_stops))
                )
            )
        if isinstance(allowed, list) and not set(stop_at).issubset(set(allowed)):
            raise CheckError(
                "role {!r} must allow every role where traversal stops".format(role)
            )

    reject_unknown_keys(rules, {"exact", "families"}, "role_rules")
    exact = rules.get("exact")
    families = rules.get("families")
    if not isinstance(exact, dict):
        raise CheckError("role_rules.exact must be an object")
    if not isinstance(families, list):
        raise CheckError("role_rules.families must be a list")

    exact_roles: Dict[str, str] = {}
    for package, role in exact.items():
        if not isinstance(package, str) or not package:
            raise CheckError("exact role rule names must be non-empty strings")
        if not isinstance(role, str) or role not in known_roles:
            raise CheckError(
                "exact role rule for {!r} references an unknown role".format(package)
            )
        exact_roles[package] = role

    family_roles: List[Tuple[str, str]] = []
    for family in families:
        if not isinstance(family, dict):
            raise CheckError("each family role rule must be an object")
        reject_unknown_keys(family, {"prefix", "role"}, "family role rule")
        prefix = family.get("prefix")
        role = family.get("role")
        if not isinstance(prefix, str) or not prefix:
            raise CheckError("each family role rule must have a non-empty prefix")
        if not isinstance(role, str) or role not in known_roles:
            raise CheckError(
                "family prefix {!r} references an unknown role".format(prefix)
            )
        family_roles.append((prefix, role))

    focused: List[FocusedClassification] = []
    focused_names: Set[str] = set()
    for value in focused_values:
        if not isinstance(value, dict):
            raise CheckError("each focused classification must be an object")
        reject_unknown_keys(
            value,
            {"package", "expected_role", "expected_origin"},
            "focused classification",
        )
        package = value.get("package")
        role = value.get("expected_role")
        origin = value.get("expected_origin")
        if not isinstance(package, str) or not package:
            raise CheckError("focused classification package must be non-empty")
        if package in focused_names:
            raise CheckError(
                "focused classification repeats package {!r}".format(package)
            )
        focused_names.add(package)
        if not isinstance(role, str) or role not in known_roles:
            raise CheckError(
                "focused package {!r} references an unknown role".format(package)
            )
        if not isinstance(origin, dict):
            raise CheckError(
                "focused package {!r} must define expected_origin".format(package)
            )
        reject_unknown_keys(
            origin, {"kind", "rule"}, "focused classification origin"
        )
        origin_kind = origin.get("kind")
        origin_rule = origin.get("rule")
        if origin_kind not in {"exact", "family"}:
            raise CheckError(
                "focused package {!r} has invalid origin kind".format(package)
            )
        if not isinstance(origin_rule, str) or not origin_rule:
            raise CheckError(
                "focused package {!r} has invalid origin rule".format(package)
            )
        focused.append(
            FocusedClassification(package, role, origin_kind, origin_rule)
        )

    return roles, exact_roles, family_roles, set(dependency_kinds), focused


def classify(
    package_name: str,
    exact_roles: Dict[str, str],
    family_roles: List[Tuple[str, str]],
) -> Optional[RoleAssignment]:
    exact = exact_roles.get(package_name)
    if exact is not None:
        return RoleAssignment(exact, "exact", package_name)

    matches = [
        (prefix, role)
        for prefix, role in family_roles
        if package_name.startswith(prefix)
    ]
    if len(matches) > 1:
        raise CheckError(
            "workspace package {!r} matches multiple family roles: {}".format(
                package_name,
                ", ".join(
                    "{!r} -> {}".format(prefix, role)
                    for prefix, role in sorted(matches)
                ),
            )
        )
    if matches:
        prefix, role = matches[0]
        return RoleAssignment(role, "family", prefix)
    if package_name == "nmp" or package_name.startswith("nmp-"):
        raise CheckError(
            "workspace package {!r} has no role classification".format(package_name)
        )
    return None


def resolved_graph(
    metadata: Dict[str, Any], checked_kinds: Set[str]
) -> Tuple[
    Dict[str, Dict[str, Any]], Set[str], Dict[str, List[str]], Dict[str, str]
]:
    package_values = metadata.get("packages")
    workspace_values = metadata.get("workspace_members")
    resolve = metadata.get("resolve")
    if not isinstance(package_values, list):
        raise CheckError("cargo metadata packages must be a list")
    if not isinstance(workspace_values, list):
        raise CheckError("cargo metadata workspace_members must be a list")
    if not isinstance(resolve, dict) or not isinstance(resolve.get("nodes"), list):
        raise CheckError("cargo metadata must contain resolve.nodes")

    packages: Dict[str, Dict[str, Any]] = {}
    for package in package_values:
        if not isinstance(package, dict):
            raise CheckError("cargo metadata contains a malformed package")
        package_id = package.get("id")
        name = package.get("name")
        if not isinstance(package_id, str) or not isinstance(name, str):
            raise CheckError("cargo metadata package must have string id and name")
        if package_id in packages:
            raise CheckError("cargo metadata repeats package id {!r}".format(package_id))
        packages[package_id] = package

    if not all(isinstance(member, str) for member in workspace_values):
        raise CheckError("cargo metadata workspace_members must contain ids")
    workspace = set(workspace_values)
    missing_members = workspace - set(packages)
    if missing_members:
        raise CheckError("workspace member ids are missing from cargo packages")

    adjacency: Dict[str, Set[str]] = {
        package_id: set() for package_id in packages
    }
    seen_nodes: Set[str] = set()
    for node in resolve["nodes"]:
        if not isinstance(node, dict) or not isinstance(node.get("id"), str):
            raise CheckError("cargo metadata contains a malformed resolve node")
        source = node["id"]
        if source not in packages:
            raise CheckError("resolve node {!r} has no package".format(source))
        if source in seen_nodes:
            raise CheckError("cargo metadata repeats resolve node {!r}".format(source))
        seen_nodes.add(source)
        deps = node.get("deps")
        if not isinstance(deps, list):
            raise CheckError("resolve node {!r} has malformed deps".format(source))
        for dependency in deps:
            if not isinstance(dependency, dict):
                raise CheckError(
                    "resolve node {!r} has a malformed dependency".format(source)
                )
            target = dependency.get("pkg")
            if not isinstance(target, str) or target not in packages:
                raise CheckError(
                    "resolve node {!r} names an unknown package".format(source)
                )
            dep_kinds = dependency.get("dep_kinds")
            if dep_kinds == []:
                dep_kinds = [{"kind": None}]
            if not isinstance(dep_kinds, list) or not dep_kinds:
                raise CheckError(
                    "resolve node {!r} has malformed dependency kinds".format(source)
                )
            include = False
            for dep_kind in dep_kinds:
                if not isinstance(dep_kind, dict):
                    raise CheckError(
                        "resolve node {!r} has a malformed dependency kind".format(
                            source
                        )
                    )
                kind = dep_kind.get("kind")
                normalized = "normal" if kind is None else kind
                if not isinstance(normalized, str):
                    raise CheckError(
                        "resolve node {!r} has an invalid dependency kind".format(
                            source
                        )
                    )
                if normalized in checked_kinds:
                    include = True
            if include:
                adjacency[source].add(target)

    missing_nodes = set(packages) - seen_nodes
    if missing_nodes:
        raise CheckError(
            "cargo metadata resolve graph omits package nodes: {}".format(
                ", ".join(sorted(missing_nodes))
            )
        )

    node_names = {package_id: package["name"] for package_id, package in packages.items()}
    ordered = {
        source: sorted(targets, key=lambda target: (node_names[target], target))
        for source, targets in adjacency.items()
    }
    return packages, workspace, ordered, node_names


def display_names(
    packages: Dict[str, Dict[str, Any]], node_names: Dict[str, str]
) -> Dict[str, str]:
    counts = Counter(node_names.values())
    return {
        package_id: (
            "{} [{}]".format(name, package_id)
            if counts[name] > 1
            else name
        )
        for package_id, name in node_names.items()
    }


def shortest_paths(
    source: str,
    adjacency: Dict[str, List[str]],
    node_names: Dict[str, str],
    traversal_stops: Set[str],
) -> Dict[str, Tuple[str, ...]]:
    source_key = (node_names[source], source)
    best: Dict[str, Tuple[int, PathKey]] = {source: (0, (source_key,))}
    paths: Dict[str, Tuple[str, ...]] = {source: (source,)}
    queue: List[Tuple[int, PathKey, str, Tuple[str, ...]]] = [
        (0, (source_key,), source, (source,))
    ]

    while queue:
        distance, path_key, current, path = heapq.heappop(queue)
        if best.get(current) != (distance, path_key):
            continue
        if current != source and current in traversal_stops:
            continue
        for target in adjacency[current]:
            target_key = (node_names[target], target)
            candidate_distance = distance + 1
            candidate_key = path_key + (target_key,)
            candidate = (candidate_distance, candidate_key)
            if target in best and best[target] <= candidate:
                continue
            target_path = path + (target,)
            best[target] = candidate
            paths[target] = target_path
            heapq.heappush(
                queue,
                (candidate_distance, candidate_key, target, target_path),
            )
    return paths


def check(
    metadata: Dict[str, Any], policy: Dict[str, Any]
) -> Tuple[List[str], List[str]]:
    (
        roles,
        exact_roles,
        family_roles,
        checked_kinds,
        focused,
    ) = parse_policy(policy)
    packages, workspace, adjacency, node_names = resolved_graph(
        metadata, checked_kinds
    )
    displays = display_names(packages, node_names)

    assignments: Dict[str, RoleAssignment] = {}
    for package_id in sorted(workspace, key=lambda item: (node_names[item], item)):
        assignment = classify(
            node_names[package_id], exact_roles, family_roles
        )
        if assignment is not None:
            assignments[package_id] = assignment

    focused_messages: List[str] = []
    for expectation in focused:
        matches = [
            package_id
            for package_id in workspace
            if node_names[package_id] == expectation.package
        ]
        if not matches:
            continue
        if len(matches) != 1:
            raise CheckError(
                "focused package {!r} appears more than once".format(
                    expectation.package
                )
            )
        assignment = assignments.get(matches[0])
        if assignment is None:
            raise CheckError(
                "focused package {!r} has no role classification".format(
                    expectation.package
                )
            )
        actual = (
            assignment.role,
            assignment.origin_kind,
            assignment.origin_rule,
        )
        expected = (
            expectation.role,
            expectation.origin_kind,
            expectation.origin_rule,
        )
        if actual != expected:
            raise CheckError(
                "focused package {!r} must be {} via {} rule {!r}; "
                "got {} via {} rule {!r}".format(
                    expectation.package,
                    expectation.role,
                    expectation.origin_kind,
                    expectation.origin_rule,
                    assignment.role,
                    assignment.origin_kind,
                    assignment.origin_rule,
                )
            )
        focused_messages.append(
            "dependency-direction: focused classification: {} [{}] via {} rule {!r}".format(
                expectation.package,
                assignment.role,
                assignment.origin_kind,
                assignment.origin_rule,
            )
        )

    violations: List[Tuple[NodeKey, NodeKey, Tuple[NodeKey, ...], str]] = []
    for source in sorted(assignments, key=lambda item: (node_names[item], item)):
        source_assignment = assignments[source]
        allowed_value = roles[source_assignment.role]["may_reach"]
        if allowed_value == "*":
            continue
        allowed_roles = set(allowed_value)
        stop_roles = set(
            roles[source_assignment.role].get("stop_at_roles", [])
        )
        traversal_stops = {
            package_id
            for package_id, assignment in assignments.items()
            if assignment.role in stop_roles
        }
        paths = shortest_paths(source, adjacency, node_names, traversal_stops)
        for target in assignments:
            if target == source or target not in paths:
                continue
            if assignments[target].role in allowed_roles:
                continue
            path = paths[target]
            path_key = tuple((node_names[item], item) for item in path)
            allowed_text = ", ".join(allowed_value)
            message = "\n".join(
                [
                    "dependency-direction: forbidden dependency path",
                    "  source: {} [{}; {} rule {!r}]".format(
                        displays[source],
                        source_assignment.role,
                        source_assignment.origin_kind,
                        source_assignment.origin_rule,
                    ),
                    "  forbidden target: {} [{}; {} rule {!r}]".format(
                        displays[target],
                        assignments[target].role,
                        assignments[target].origin_kind,
                        assignments[target].origin_rule,
                    ),
                    "  shortest path: {}".format(
                        " -> ".join(displays[item] for item in path)
                    ),
                    "  allowed target roles for {}: {}".format(
                        source_assignment.role, allowed_text
                    ),
                ]
            )
            violations.append(
                (
                    (node_names[source], source),
                    (node_names[target], target),
                    path_key,
                    message,
                )
            )

    violations.sort(key=lambda value: (value[0], value[1], value[2]))
    return focused_messages, [value[3] for value in violations]


def main() -> int:
    if len(sys.argv) != 3:
        print(
            "usage: {} POLICY.json CARGO-METADATA.json".format(
                Path(sys.argv[0]).name
            ),
            file=sys.stderr,
        )
        return 2

    policy_path = Path(sys.argv[1])
    metadata_path = Path(sys.argv[2])
    try:
        policy = load_json(policy_path)
        metadata = load_json(metadata_path)
        focused_messages, violations = check(metadata, policy)
    except CheckError as error:
        print("dependency-direction: {}".format(error), file=sys.stderr)
        return 1

    output = sys.stderr if violations else sys.stdout
    for message in focused_messages:
        print(message, file=output)
    if violations:
        print("\n\n".join(violations), file=sys.stderr)
        return 1

    print(
        "dependency-direction: ok ({} workspace packages)".format(
            len(metadata["workspace_members"])
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
