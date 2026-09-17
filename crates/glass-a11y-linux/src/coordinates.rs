use glass_core::{AxNode, AxRect, AxRole};

/// The accessible top-level window's origin in the provider's window coordinate space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowOrigin {
    x: i32,
    y: i32,
}

impl WindowOrigin {
    pub(crate) fn from_bounds(bounds: AxRect) -> Self {
        Self {
            x: bounds.x,
            y: bounds.y,
        }
    }

    pub(crate) fn normalize(self, bounds: AxRect) -> Option<AxRect> {
        Some(AxRect {
            x: bounds.x.checked_sub(self.x)?,
            y: bounds.y.checked_sub(self.y)?,
            ..bounds
        })
    }

    pub(crate) fn provider_point(self, point: (i32, i32)) -> Option<(i32, i32)> {
        Some((point.0.checked_add(self.x)?, point.1.checked_add(self.y)?))
    }
}

pub(crate) fn normalize_tree(node: &mut AxNode) {
    if matches!(node.role, AxRole::Window | AxRole::Dialog) {
        // Firefox on Wayland includes an offset that capture/input do not include.
        let origin = node.bounds.map(WindowOrigin::from_bounds);
        normalize_subtree(node, origin);
    } else {
        for child in &mut node.children {
            normalize_tree(child);
        }
    }
}

fn normalize_subtree(node: &mut AxNode, origin: Option<WindowOrigin>) {
    node.bounds = origin.and_then(|origin| node.bounds.and_then(|b| origin.normalize(b)));
    // Embedded dialogs and web frames share their containing top-level window's origin.
    for child in &mut node.children {
        normalize_subtree(child, origin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{AxNodeId, AxStates};

    fn rect(x: i32, y: i32) -> AxRect {
        AxRect {
            x,
            y,
            width: 90,
            height: 36,
        }
    }

    fn node(role: AxRole, bounds: Option<AxRect>, children: Vec<AxNode>) -> AxNode {
        AxNode {
            id: AxNodeId(7),
            role,
            raw_role: String::new(),
            name: Some("target".into()),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds,
            children,
        }
    }

    #[test]
    fn firefox_bounds_and_hit_points_use_inverse_translations() {
        let origin = WindowOrigin::from_bounds(rect(26, 23));
        assert_eq!(origin.normalize(rect(50, 297)), Some(rect(24, 274)));
        assert_eq!(origin.provider_point((69, 292)), Some((95, 315)));
    }

    #[test]
    fn zero_origin_preserves_native_and_x11_geometry() {
        let origin = WindowOrigin::from_bounds(rect(0, 0));
        for bounds in [rect(13, 30), rect(-20, -40)] {
            assert_eq!(origin.normalize(bounds), Some(bounds));
            assert_eq!(
                origin.provider_point((bounds.x, bounds.y)),
                Some((bounds.x, bounds.y))
            );
        }
    }

    #[test]
    fn embedded_dialogs_share_the_window_origin_but_sibling_windows_do_not() {
        let dialog = node(
            AxRole::Dialog,
            Some(rect(100, 100)),
            vec![node(AxRole::Button, Some(rect(120, 140)), vec![])],
        );
        let mut app = node(
            AxRole::Application,
            None,
            vec![
                node(
                    AxRole::Window,
                    Some(rect(26, 23)),
                    vec![node(AxRole::Document, Some(rect(26, 109)), vec![dialog])],
                ),
                node(
                    AxRole::Dialog,
                    Some(rect(-10, -20)),
                    vec![node(AxRole::Button, Some(rect(5, 15)), vec![])],
                ),
            ],
        );
        normalize_tree(&mut app);
        let document = &app.children[0].children[0];
        assert_eq!(document.bounds, Some(rect(0, 86)));
        assert_eq!(document.children[0].bounds, Some(rect(74, 77)));
        assert_eq!(document.children[0].children[0].bounds, Some(rect(94, 117)));
        assert_eq!(app.children[1].children[0].bounds, Some(rect(15, 35)));
        assert_eq!(app.children[1].children[0].id, AxNodeId(7));
    }

    #[test]
    fn unreadable_window_origin_withholds_descendant_coordinates() {
        let mut window = node(
            AxRole::Window,
            None,
            vec![node(
                AxRole::Dialog,
                Some(rect(100, 100)),
                vec![node(AxRole::Button, Some(rect(120, 140)), vec![])],
            )],
        );
        normalize_tree(&mut window);
        assert!(window.children[0].bounds.is_none());
        assert!(window.children[0].children[0].bounds.is_none());
        assert_eq!(
            window.children[0].children[0].name.as_deref(),
            Some("target")
        );
    }

    #[test]
    fn coordinates_outside_the_integer_range_are_not_clamped_or_wrapped() {
        let origin = WindowOrigin::from_bounds(rect(26, 23));
        assert!(origin.normalize(rect(i32::MIN, 0)).is_none());
        assert!(origin.normalize(rect(0, i32::MIN)).is_none());
        assert!(origin.provider_point((i32::MAX, 0)).is_none());
        assert!(origin.provider_point((0, i32::MAX)).is_none());
    }
}
