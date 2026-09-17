//! Reconcile an idb point query with a fresh tree using process-scoped accessibility identifiers.

use glass_core::{AxContext, AxNodeId, AxTarget, AxTree, GlassError, PointerHit, Result};
use serde_json::Value;

use crate::axmap;

#[derive(Default)]
struct Metadata {
    pid: Option<u32>,
    identifier: Option<String>,
    frame: Option<[f64; 4]>,
}

impl Metadata {
    fn read(node: &Value, scale: f64) -> Self {
        Self {
            pid: node
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid != 0),
            identifier: node
                .get("AXUniqueId")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
                .map(str::to_owned),
            frame: checked_frame(node.get("frame"), scale),
        }
    }

    fn key(&self) -> Option<(u32, &str)> {
        Some((self.pid?, self.identifier.as_deref()?))
    }
}

fn checked_frame(frame: Option<&Value>, scale: f64) -> Option<[f64; 4]> {
    let frame = frame?;
    let get = |key| frame.get(key).and_then(Value::as_f64);
    let points = [get("x")?, get("y")?, get("width")?, get("height")?];
    let [x, y, w, h] = points.map(|value| (value * scale).round());
    (scale.is_finite()
        && scale > 0.0
        && x >= f64::from(i32::MIN)
        && x <= f64::from(i32::MAX)
        && y >= f64::from(i32::MIN)
        && y <= f64::from(i32::MAX)
        && w > 0.0
        && w <= f64::from(u32::MAX)
        && h > 0.0
        && h <= f64::from(u32::MAX))
    .then_some(points)
}

struct ProbeTree {
    tree: AxTree,
    metadata: Vec<Metadata>,
}

impl ProbeTree {
    fn parse(json: &str, scale: f64, ctx: &AxContext) -> Result<Self> {
        let mut metadata = vec![Metadata::default()];
        let mut tree =
            axmap::build_tree_observing(json, scale, &ctx.window, ctx.limits, &mut |node| {
                metadata.push(Metadata::read(node, scale))
            })?;
        tree.assign_ids();
        Ok(Self { tree, metadata })
    }

    fn unique_id(&self, key: (u32, &str)) -> Option<AxNodeId> {
        let mut matches = self.metadata.iter().enumerate().filter(|(_, metadata)| {
            metadata.identifier.as_deref() == Some(key.1)
                && metadata.pid.is_none_or(|pid| pid == key.0)
        });
        let (id, metadata) = matches.next()?;
        if metadata.key() != Some(key) || matches.next().is_some() {
            return None;
        }
        u32::try_from(id).ok().map(AxNodeId)
    }
}

pub(crate) fn classify(
    tree_json: &str,
    hit_json: &str,
    scale: f64,
    ctx: &AxContext,
    target: &AxTarget,
) -> Result<PointerHit> {
    let current = ProbeTree::parse(tree_json, scale, ctx)?;
    let hit = if hit_json.trim() == "null" {
        None
    } else {
        Some(ProbeTree::parse(hit_json, scale, ctx)?)
    };
    let changed = || GlassError::AxElementChanged(target.id.0);
    let node = current.tree.find(target.id).ok_or_else(changed)?;
    if !target.matches(node.role, node.name.as_deref())
        || target.bounds.is_none()
        || !target.bounds_consistent(node.bounds, 0)
        || !node.states.enabled
    {
        return Err(changed());
    }
    if !current.tree.is_complete() {
        return Ok(PointerHit::Inconclusive);
    }
    let target_metadata = &current.metadata[target.id.0 as usize];
    let Some(target_key) = target_metadata.key() else {
        return Ok(PointerHit::Inconclusive);
    };
    if target_metadata.frame.is_none() || current.unique_id(target_key) != Some(target.id) {
        return Ok(PointerHit::Inconclusive);
    }
    let Some(hit) = hit else {
        return Ok(PointerHit::Inconclusive);
    };
    let [hit_node] = hit.tree.root.children.as_slice() else {
        return Ok(PointerHit::Inconclusive);
    };
    if !hit.tree.is_complete() {
        return Ok(PointerHit::Inconclusive);
    }
    let hit_metadata = &hit.metadata[hit_node.id.0 as usize];
    let Some(hit_key) = hit_metadata.key() else {
        return Ok(PointerHit::Inconclusive);
    };
    let Some(hit_id) = current.unique_id(hit_key) else {
        return Ok(PointerHit::Inconclusive);
    };
    let observed = current.tree.find(hit_id).ok_or_else(changed)?;
    let observed_metadata = &current.metadata[hit_id.0 as usize];
    if hit_metadata.frame.is_none() || observed_metadata.frame.is_none() {
        return Ok(PointerHit::Inconclusive);
    }
    if hit_metadata.frame != observed_metadata.frame
        || hit_node.role != observed.role
        || hit_node.raw_role != observed.raw_role
        || hit_node.name != observed.name
        || hit_node.states != observed.states
        || hit_node.bounds != observed.bounds
    {
        return Err(changed());
    }

    let hit_path = current.tree.path_to(hit_id).ok_or_else(changed)?;
    if !node.role.is_interactable() {
        let target_path = current.tree.path_to(target.id).ok_or_else(changed)?;
        if target_path
            .iter()
            .rev()
            .skip(1)
            .find(|ancestor| ancestor.role.is_interactable())
            .is_some_and(|ancestor| ancestor.id == hit_id)
        {
            return Ok(PointerHit::AcceptedAncestor);
        }
    }
    for ancestor in hit_path.iter().rev() {
        if ancestor.id == target.id {
            return Ok(PointerHit::Target);
        }
        if ancestor.role.is_interactable() {
            return Ok(PointerHit::Other);
        }
    }
    Ok(PointerHit::Other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{Deadline, WalkLimits, WindowGeometry};
    use serde_json::json;

    fn ctx() -> AxContext {
        AxContext {
            pids: vec![42],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 400,
                height: 800,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: Deadline::UNBOUNDED,
        }
    }

    fn button(identifier: &str) -> Value {
        json!({"pid":42,"AXUniqueId":identifier,"AXLabel":identifier,"role":"AXButton",
            "enabled":true,"frame":{"x":20,"y":30,"width":80,"height":30},"children":[]})
    }

    fn target(tree: &Value, name: &str) -> AxTarget {
        let parsed = ProbeTree::parse(&tree.to_string(), 2.0, &ctx()).unwrap();
        let node = parsed
            .tree
            .find_first(|node| node.name.as_deref() == Some(name))
            .unwrap();
        AxTarget {
            id: node.id,
            role: node.role,
            name: node.name.clone(),
            bounds: node.bounds,
            value: node.value.clone(),
        }
    }

    fn probe(tree: &Value, hit: &Value, target: &AxTarget) -> Result<PointerHit> {
        classify(&tree.to_string(), &hit.to_string(), 2.0, &ctx(), target)
    }

    #[test]
    fn distinct_identifier_with_identical_bounds_is_a_cover() {
        let hit = button("cover");
        let tree = json!([button("target"), hit]);
        assert_eq!(
            probe(&tree, &hit, &target(&tree, "target")).unwrap(),
            PointerHit::Other
        );
    }

    #[test]
    fn narrower_cover_is_other_and_uncovered_target_is_proven() {
        let mut cover = button("cover");
        cover["frame"]["x"] = json!(50);
        cover["frame"]["width"] = json!(20);
        let tree = json!([button("target"), cover]);
        let target = target(&tree, "target");
        assert_eq!(probe(&tree, &cover, &target).unwrap(), PointerHit::Other);
        assert_eq!(probe(&tree, &tree[0], &target).unwrap(), PointerHit::Target);
    }

    #[test]
    fn repeated_identifiers_never_prove_target_or_cover() {
        for name in ["target", "cover"] {
            let mut duplicate = button(name);
            duplicate["frame"]["y"] = json!(100);
            let tree = json!([button("target"), button("cover"), duplicate]);
            let hit = if name == "target" { &tree[0] } else { &tree[1] };
            assert_eq!(
                probe(&tree, hit, &target(&tree, "target")).unwrap(),
                PointerHit::Inconclusive,
                "{name}"
            );
        }
    }

    #[test]
    fn label_and_rectangle_without_identifier_do_not_prove_identity() {
        let mut node = button("target");
        node["AXUniqueId"] = Value::Null;
        let tree = json!([node]);
        assert_eq!(
            probe(&tree, &node, &target(&tree, "target")).unwrap(),
            PointerHit::Inconclusive
        );
    }

    #[test]
    fn missing_pid_or_a_hit_absent_from_the_tree_remains_unproven() {
        let tree = json!([button("target")]);
        for hit in [
            json!(null),
            json!([]),
            button("absent"),
            json!([button("target"), button("target")]),
        ] {
            assert_eq!(
                probe(&tree, &hit, &target(&tree, "target")).unwrap(),
                PointerHit::Inconclusive
            );
        }
        let mut hit = tree[0].clone();
        hit["pid"] = Value::Null;
        assert_eq!(
            probe(&tree, &hit, &target(&tree, "target")).unwrap(),
            PointerHit::Inconclusive
        );
    }

    #[test]
    fn identical_identifiers_in_different_processes_are_distinct() {
        let mut other_app = button("target");
        other_app["pid"] = json!(43);
        let tree = json!([button("target"), other_app]);
        assert_eq!(
            probe(&tree, &other_app, &target(&tree, "target")).unwrap(),
            PointerHit::Other
        );
    }

    #[test]
    fn target_geometry_or_identity_drift_refuses() {
        let original = json!([button("target")]);
        let target = target(&original, "target");
        for (field, value) in [
            ("AXUniqueId", json!("replacement")),
            ("enabled", json!(false)),
            ("frame", json!({"x":21,"y":30,"width":80,"height":30})),
        ] {
            let mut tree = original.clone();
            tree[0][field] = value;
            assert!(
                matches!(
                    probe(&tree, &tree[0], &target),
                    Err(GlassError::AxElementChanged(_))
                ),
                "{field}"
            );
        }
    }

    #[test]
    fn hit_geometry_changed_between_the_point_query_and_tree_refuses() {
        let hit = button("target");
        let mut current = hit.clone();
        current["frame"]["x"] = json!(20.1);
        let tree = json!([current]);
        let target = target(&tree, "target");
        assert!(matches!(
            probe(&tree, &hit, &target),
            Err(GlassError::AxElementChanged(_))
        ));
    }

    #[test]
    fn truncated_tree_cannot_establish_identifier_uniqueness() {
        let tree = json!([button("target"), button("target")]);
        let target = target(&tree, "target");
        let mut ctx = ctx();
        ctx.limits.nodes = 1;
        assert_eq!(
            classify(&tree.to_string(), &tree[0].to_string(), 2.0, &ctx, &target).unwrap(),
            PointerHit::Inconclusive
        );
    }

    #[test]
    fn malformed_json_is_an_error_and_invalid_bounds_are_unproven() {
        let tree = json!([button("target")]);
        let target = target(&tree, "target");
        assert!(classify(&tree.to_string(), "{", 2.0, &ctx(), &target).is_err());
        let mut hit = tree[0].clone();
        hit["frame"]["width"] = json!(-1);
        assert_eq!(
            probe(&tree, &hit, &target).unwrap(),
            PointerHit::Inconclusive
        );
    }

    #[test]
    fn descendant_hit_reaches_target_but_nested_interactive_control_is_other() {
        let mut child = button("child");
        child["role"] = json!("AXStaticText");
        let mut parent = button("target");
        parent["children"] = json!([child]);
        let tree = json!([parent]);
        assert_eq!(
            probe(&tree, &child, &target(&tree, "target")).unwrap(),
            PointerHit::Target
        );

        let child = button("child");
        parent["children"] = json!([child]);
        let tree = json!([parent]);
        assert_eq!(
            probe(&tree, &child, &target(&tree, "target")).unwrap(),
            PointerHit::Other
        );
    }

    #[test]
    fn passive_target_accepts_its_nearest_interactive_ancestor() {
        let mut child = button("label");
        child["role"] = json!("AXStaticText");
        let mut parent = button("parent");
        parent["children"] = json!([child]);
        let tree = json!([parent]);
        assert_eq!(
            probe(&tree, &parent, &target(&tree, "label")).unwrap(),
            PointerHit::AcceptedAncestor
        );
    }
}
