use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use glass_core::{
    ActionMode, ActionTarget, Backend, BaselineStore, BoundDispatch, Glass, PlatformFactory,
};

use super::{
    ValidatedType, validate_action, validate_click_element_args, validate_set_value_args,
    validate_type_args,
};
use crate::params::{Action, ClickElementArgs, SetValueArgs, TypeArgs};
use crate::tools::{
    ContextualError, ToolContext, click_element_with, set_value_with, type_text_with,
};

fn panicking_glass() -> (Glass, Arc<AtomicBool>) {
    let touched = Arc::new(AtomicBool::new(false));
    let factory_touched = touched.clone();
    let factory: PlatformFactory = Box::new(move |_| -> glass_core::Result<Backend> {
        factory_touched.store(true, Ordering::SeqCst);
        panic!("invalid request reached the platform factory")
    });
    let root = tempfile::tempdir().unwrap().path().join("baselines");
    (
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100),
        touched,
    )
}

enum InvalidHandlerArgs {
    Click(ClickElementArgs),
    SetValue(SetValueArgs),
    Type(TypeArgs),
}

fn assert_invalid_handler(name: &str, args: InvalidHandlerArgs, expected: &str) {
    let (mut glass, touched) = panicking_glass();
    let result = match args {
        InvalidHandlerArgs::Click(args) => {
            click_element_with(&mut glass, &args, ToolContext::UNBOUNDED)
        }
        InvalidHandlerArgs::SetValue(args) => {
            set_value_with(&mut glass, &args, ToolContext::UNBOUNDED)
        }
        InvalidHandlerArgs::Type(args) => type_text_with(&mut glass, &args, ToolContext::UNBOUNDED),
    };
    let error = result.unwrap_err();
    assert!(
        error.message.contains(expected),
        "{name}: expected {expected:?}, got {:?}",
        error.message
    );
    assert_eq!(
        error.bound_dispatch,
        Some(BoundDispatch::NotDispatched),
        "{name}: validation must be proven pre-dispatch"
    );
    assert!(
        !touched.load(Ordering::SeqCst),
        "{name}: validation touched the platform factory"
    );
}

#[test]
fn invalid_semantic_handler_arguments_fail_before_session_io() {
    let click =
        |json| InvalidHandlerArgs::Click(serde_json::from_str::<ClickElementArgs>(json).unwrap());
    let set_value =
        |json| InvalidHandlerArgs::SetValue(serde_json::from_str::<SetValueArgs>(json).unwrap());
    let type_text =
        |json| InvalidHandlerArgs::Type(serde_json::from_str::<TypeArgs>(json).unwrap());

    let rows = [
        ("click neither", click(r#"{}"#), "exactly one"),
        (
            "click both",
            click(r#"{"id":1,"target":{"role":"Button"}}"#),
            "exactly one",
        ),
        (
            "click id timeout",
            click(r#"{"id":1,"timeout_ms":1}"#),
            "timeout_ms",
        ),
        (
            "click id max nodes",
            click(r#"{"id":1,"max_nodes":0}"#),
            "max_nodes",
        ),
        (
            "click selector timeout too large",
            click(r#"{"target":{"role":"Button"},"timeout_ms":120001}"#),
            "120000",
        ),
        (
            "click empty target",
            click(r#"{"target":{}}"#),
            "specify query",
        ),
        (
            "click unknown role",
            click(r#"{"target":{"role":"Mystery"}}"#),
            "unknown role",
        ),
        (
            "click unknown state",
            click(r#"{"target":{"states":["sparkling"]}}"#),
            "unknown state",
        ),
        (
            "click contradictory states",
            click(r#"{"target":{"states":["enabled","disabled"]}}"#),
            "contradict",
        ),
        (
            "click unknown return",
            click(r#"{"id":1,"return":"later"}"#),
            "unknown return",
        ),
        ("set neither", set_value(r#"{"text":"Ada"}"#), "exactly one"),
        (
            "set both",
            set_value(r#"{"id":1,"target":{"role":"TextField"},"text":"Ada"}"#),
            "exactly one",
        ),
        (
            "set id timeout",
            set_value(r#"{"id":1,"text":"Ada","timeout_ms":1}"#),
            "timeout_ms",
        ),
        (
            "set id max nodes",
            set_value(r#"{"id":1,"text":"Ada","max_nodes":0}"#),
            "max_nodes",
        ),
        (
            "set unknown role",
            set_value(r#"{"target":{"role":"Mystery"},"text":"Ada"}"#),
            "unknown role",
        ),
        (
            "set unknown return",
            set_value(r#"{"id":1,"text":"Ada","return":"later"}"#),
            "unknown return",
        ),
        (
            "untargeted type focus mode",
            type_text(r#"{"text":"Ada","focus_mode":"auto"}"#),
            "focus_mode",
        ),
        (
            "untargeted type timeout",
            type_text(r#"{"text":"Ada","timeout_ms":1}"#),
            "timeout_ms",
        ),
        (
            "untargeted type max nodes",
            type_text(r#"{"text":"Ada","max_nodes":0}"#),
            "max_nodes",
        ),
        (
            "targeted type timeout too large",
            type_text(r#"{"target":{"role":"TextField"},"text":"Ada","timeout_ms":120001}"#),
            "120000",
        ),
        (
            "targeted type unknown state",
            type_text(r#"{"target":{"states":["sparkling"]},"text":"Ada"}"#),
            "unknown state",
        ),
        (
            "type unknown return",
            type_text(r#"{"text":"Ada","return":"later"}"#),
            "unknown return",
        ),
    ];

    for (name, args, expected) in rows {
        assert_invalid_handler(name, args, expected);
    }
}

#[test]
fn invalid_typed_modes_and_recursive_or_misspelled_scopes_fail_deserialization() {
    assert!(serde_json::from_str::<ClickElementArgs>(r#"{"id":1,"mode":"magic"}"#).is_err());
    assert!(
        serde_json::from_str::<TypeArgs>(
            r#"{"target":{"role":"TextField"},"text":"Ada","focus_mode":"magic"}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<Action>(r#"{"action":"click_element","id":1,"mode":"magic"}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<Action>(
            r#"{"action":"type","target":{"role":"TextField"},"text":"Ada","focus_mode":"magic"}"#
        )
        .is_err()
    );

    for json in [
        r#"{"target":{"role":"Button","within":{"role":"Document","within":{"role":"Group"}}}}"#,
        r#"{"target":{"role":"Button","statse":["enabled"]}}"#,
        r#"{"target":{"role":"Button","within":{"rol":"Document"}}}"#,
    ] {
        assert!(
            serde_json::from_str::<ClickElementArgs>(json).is_err(),
            "recursive or misspelled scope unexpectedly parsed: {json}"
        );
    }
}

#[test]
fn valid_target_forms_bind_legacy_and_semantic_defaults() {
    let legacy: ClickElementArgs = serde_json::from_str(r#"{"id":42,"mode":"pointer"}"#).unwrap();
    let legacy = validate_click_element_args(&legacy).unwrap();
    assert!(matches!(legacy.target, ActionTarget::Id(id) if id.0 == 42));
    assert_eq!(legacy.mode, ActionMode::Pointer);
    assert_eq!(legacy.timeout_ms, None);
    assert_eq!(legacy.max_nodes, None);

    let semantic: ClickElementArgs =
        serde_json::from_str(r#"{"target":{"role":"Button"}}"#).unwrap();
    let semantic = validate_click_element_args(&semantic).unwrap();
    assert!(matches!(semantic.target, ActionTarget::Semantic(_)));
    assert_eq!(semantic.mode, ActionMode::Auto);
    assert_eq!(semantic.timeout_ms, Some(10_000));
    assert_eq!(semantic.max_nodes, None);

    let zero: SetValueArgs =
        serde_json::from_str(r#"{"target":{"role":"TextField"},"text":"Ada","timeout_ms":0}"#)
            .unwrap();
    let zero = validate_set_value_args(&zero).unwrap();
    assert_eq!(zero.timeout_ms, Some(0));
    assert_eq!(zero.max_nodes, None);

    let targeted: TypeArgs =
        serde_json::from_str(r#"{"target":{"role":"TextField"},"text":"Ada"}"#).unwrap();
    let ValidatedType::Targeted(targeted) = validate_type_args(&targeted).unwrap() else {
        panic!("targeted type must bind targeted params")
    };
    assert_eq!(targeted.focus_mode, ActionMode::Auto);
    assert_eq!(targeted.timeout_ms, 10_000);
    assert_eq!(targeted.max_nodes, None);

    let untargeted: TypeArgs =
        serde_json::from_str(r#"{"text":"Ada","return":"snapshot"}"#).unwrap();
    assert!(matches!(
        validate_type_args(&untargeted).unwrap(),
        ValidatedType::Untargeted
    ));
}

fn action(json: &str) -> Action {
    serde_json::from_str(json).unwrap()
}

#[test]
fn validate_action_accepts_every_variant_without_session_state() {
    for action in [
        action(r#"{"action":"click","x":1,"y":2}"#),
        action(r#"{"action":"move","x":1,"y":2}"#),
        action(r#"{"action":"drag","x1":1,"y1":2,"x2":3,"y2":4}"#),
        action(r#"{"action":"scroll","x":1,"y":2}"#),
        action(r#"{"action":"type","text":"Ada"}"#),
        action(r#"{"action":"key","chord":"Return"}"#),
        action(r#"{"action":"settle"}"#),
        action(r#"{"action":"click_element","id":1}"#),
        action(r#"{"action":"set_value","id":1,"text":"Ada"}"#),
        action(r#"{"action":"wait_for_element","role":"Button"}"#),
        action(r#"{"action":"scroll_to_element","role":"Button"}"#),
    ] {
        validate_action(&action).unwrap();
    }
}

#[test]
fn validate_action_routes_each_invalid_variant_to_its_pure_helper() {
    let rows = [
        (
            action(r#"{"action":"click","x":1,"y":2,"count":0}"#),
            "count",
        ),
        (
            action(r#"{"action":"drag","x1":1,"y1":2,"x2":3,"y2":4,"button":"bad"}"#),
            "button",
        ),
        (
            action(r#"{"action":"scroll","x":1,"y":2,"dx":101}"#),
            "between",
        ),
        (
            action(r#"{"action":"type","text":"Ada","focus_mode":"auto"}"#),
            "focus_mode",
        ),
        (action(r#"{"action":"key","chord":"ctrl+"}"#), "key"),
        (
            action(r#"{"action":"settle","interval_ms":0}"#),
            "interval_ms",
        ),
        (
            action(r#"{"action":"settle","stability_region":{"x":0,"y":0,"width":0,"height":1}}"#),
            "stability_region",
        ),
        (
            action(r#"{"action":"settle","ignore":[{"x":0,"y":0,"width":1,"height":0}]}"#),
            "ignore",
        ),
        (action(r#"{"action":"click_element"}"#), "exactly one"),
        (
            action(r#"{"action":"set_value","text":"Ada"}"#),
            "exactly one",
        ),
        (
            action(r#"{"action":"wait_for_element","role":"Mystery"}"#),
            "unknown role",
        ),
        (
            action(r#"{"action":"scroll_to_element","role":"Button","x":1}"#),
            "both",
        ),
    ];
    for (action, expected) in rows {
        let error: ContextualError = validate_action(&action).unwrap_err();
        assert!(
            error.message.contains(expected),
            "expected {expected:?}, got {:?}",
            error.message
        );
        assert_eq!(error.bound_dispatch, Some(BoundDispatch::NotDispatched));
    }
}
