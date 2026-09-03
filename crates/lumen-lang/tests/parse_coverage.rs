//! Parser tests: the rejections, and the constructs the end-to-end tests never
//! reach.
//!
//! `tests/compile.rs` drives whole programs through the VM, which only exercises
//! source the parser accepts. Every branch in the parser that ends in a
//! diagnostic is a product surface of its own — "unexpected token" with no help
//! line is where a newcomer gives up — so each one is pinned here by its exact
//! message *and* its exact help line, not merely by "parsing failed".
//!
//! The tests call `parse` rather than `compile`, so a construct the emitter has
//! not implemented yet (`sim`) can still have its grammar pinned.

use lumen_lang::ast::{ChanType, Decl, ExprKind, SimStmt, Type, UnOp};
use lumen_lang::lex::Unit;
use lumen_lang::parse;

/// Every diagnostic from a parse, as `(message, help)` in the order reported.
fn diags(src: &str) -> Vec<(String, String)> {
    let (_, ds) = parse(src);
    ds.items
        .iter()
        .map(|d| (d.message.clone(), d.help.clone()))
        .collect()
}

/// The first diagnostic, which for these tests is the one being pinned.
fn first(src: &str) -> (String, String) {
    let ds = diags(src);
    assert!(!ds.is_empty(), "expected a diagnostic for:\n{src}");
    ds[0].clone()
}

/// Assert that `src` reports exactly this message and help, first.
#[track_caller]
fn rejects(src: &str, message: &str, help: &str) {
    let (m, h) = first(src);
    assert_eq!(m, message, "message for:\n{src}");
    assert_eq!(h, help, "help for:\n{src}");
}

/// Parse something that must succeed, and hand back the declarations.
#[track_caller]
fn decls(src: &str) -> Vec<Decl> {
    let (file, ds) = parse(src);
    assert!(!ds.has_errors(), "unexpected errors: {:?}", ds.items);
    file.expect("a file with no errors must parse").decls
}

/// The single effect in a source that declares one.
#[track_caller]
fn effect_of(src: &str) -> lumen_lang::ast::Effect {
    for d in decls(src) {
        if let Decl::Effect(e) = d {
            return e;
        }
    }
    panic!("no effect in:\n{src}");
}

// ---- the header ------------------------------------------------------------

#[test]
fn the_language_version_must_be_a_number() {
    rejects(
        "lumen one\n",
        "expected the language version, found an identifier",
        "the language version is a whole number",
    );
}

#[test]
fn a_language_version_with_a_unit_is_refused() {
    // `lumen 1s` is a typo, not a request for a one-second language.
    rejects(
        "lumen 1s\n",
        "the language version may not have a unit",
        "write a plain number",
    );
}

#[test]
fn a_fractional_language_version_is_refused() {
    rejects(
        "lumen 1.5\n",
        "the language version must be a whole number",
        "write a non-negative integer",
    );
}

#[test]
fn a_second_statement_on_the_header_line_is_refused_not_skipped() {
    // Silently accepting the rest of the line is how a construct the compiler
    // does not understand gets ignored instead of reported.
    rejects(
        "lumen 1 effect \"x\" {}\n",
        "unexpected an identifier after the end of a statement",
        "statements end at a newline; put this on its own line",
    );
}

#[test]
fn a_declaration_must_start_with_a_keyword() {
    rejects(
        "lumen 1\n42\n",
        "expected a declaration, found a number",
        "a file contains `effect`, `palette`, `curve` and `fn` declarations",
    );
}

#[test]
fn an_unknown_declaration_names_what_is_allowed() {
    rejects(
        "lumen 1\nwidget foo {}\n",
        "unknown declaration `widget`",
        "a file contains `effect`, `palette`, `curve` and `fn` declarations",
    );
}

// ---- effect shape ----------------------------------------------------------

#[test]
fn an_effect_name_must_be_a_string() {
    rejects(
        "lumen 1\neffect x {}\n",
        "expected the effect name, found an identifier",
        "the effect name is written in double quotes",
    );
}

#[test]
fn an_effect_without_an_opening_brace_says_which_token_it_wanted() {
    rejects(
        "lumen 1\neffect \"x\" layer\n",
        "expected `{`, found an identifier",
        "add `{` here",
    );
}

#[test]
fn an_unclosed_effect_block_is_reported_at_the_effect() {
    rejects(
        "lumen 1\neffect \"x\" {\n  layer b { color = rgb(0,0,0) }\n",
        "unclosed `effect` block",
        "add a closing `}`",
    );
}

#[test]
fn an_effect_item_must_start_with_a_keyword() {
    rejects(
        "lumen 1\neffect \"x\" {\n  42\n}\n",
        "expected an effect item, found a number",
        "inside an effect you can write `param`, `channel`, `let`, `mask`, `state`, `layer`, `sim` or `fn`",
    );
}

#[test]
fn an_unknown_capability_lists_the_real_ones() {
    rejects(
        "lumen 1\neffect \"x\" {\n  requires teleport\n}\n",
        "unknown capability `teleport`",
        "capabilities are mapped, rough, rgbw, cct, audio, imu, grid and input",
    );
}

#[test]
fn several_capabilities_may_be_listed_on_one_line() {
    use lumen_lang::ast::Cap;
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  requires grid, audio\n  layer b { color = rgb(0,0,0) }\n}\n",
    );
    assert_eq!(e.requires, vec![Cap::Grid, Cap::Audio]);
}

#[test]
fn a_capability_list_stops_at_the_first_non_name() {
    // The `None => break` arm: without it the loop would spin on the bad token.
    rejects(
        "lumen 1\neffect \"x\" {\n  requires grid, 7\n}\n",
        "expected a capability, found a number",
        "a capability must be a name like `my_capability`",
    );
}

#[test]
fn a_budget_without_a_device_class_is_refused() {
    // A budget with no class claims nothing, and admission control would read
    // it as "fits everywhere".
    rejects(
        "lumen 1\neffect \"x\" {\n  budget 900\n}\n",
        "expected `on` after a budget",
        "write `budget 900 on esp32c3`",
    );
}

#[test]
fn a_budget_claim_carries_its_class() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  budget 900 on esp32c3\n  layer b { color = rgb(0,0,0) }\n}\n",
    );
    assert_eq!(e.budgets.len(), 1);
    assert_eq!(e.budgets[0].instructions, 900);
    assert_eq!(e.budgets[0].device_class, "esp32c3");
}

#[test]
fn the_effect_header_fields_are_kept() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  version 3\n  author \"someone\"\n  stdlib 1\n  fps 60\n  layer b { color = rgb(0,0,0) }\n}\n",
    );
    assert_eq!(e.version, Some(3));
    assert_eq!(e.author.as_deref(), Some("someone"));
    assert_eq!(e.stdlib, Some(1));
    assert_eq!(e.fps, Some(60));
}

#[test]
fn an_author_must_be_a_string() {
    rejects(
        "lumen 1\neffect \"x\" {\n  author someone\n}\n",
        "expected the author name, found an identifier",
        "the author name is written in double quotes",
    );
}

#[test]
fn an_unknown_effect_item_lists_the_real_ones() {
    rejects(
        "lumen 1\neffect \"x\" {\n  frobnicate 3\n}\n",
        "unknown effect item `frobnicate`",
        "inside an effect you can write `version`, `author`, `stdlib`, `requires`, `fps`, `budget`, `param`, `channel`, `let`, `mask`, `state`, `layer`, `sim` or `fn`",
    );
}

// ---- param -----------------------------------------------------------------

#[test]
fn a_parameter_needs_a_name() {
    rejects(
        "lumen 1\neffect \"x\" {\n  param 5 : float = 1\n}\n",
        "expected a parameter name, found a number",
        "a parameter name must be a name like `my_parameter`",
    );
}

#[test]
fn an_unknown_parameter_unit_lists_the_real_ones() {
    rejects(
        "lumen 1\neffect \"x\" {\n  param p : float = 1 range 0..2 unit furlongs\n}\n",
        "unknown unit `furlongs`",
        "units are m, s, ms, deg, rad, hz and %",
    );
}

#[test]
fn a_parameters_modifiers_are_all_kept_in_any_order() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  param p : float = 1 label \"Speed\" step 0.1 unit hz range 0..2\n  layer b { color = rgb(p, 0, 0) }\n}\n",
    );
    let p = &e.params[0];
    assert_eq!(p.label.as_deref(), Some("Speed"));
    assert_eq!(p.unit, Some(Unit::Hz));
    assert!(p.step.is_some());
    assert!(p.range.is_some());
}

#[test]
fn a_word_that_is_not_a_parameter_modifier_ends_the_statement() {
    rejects(
        "lumen 1\neffect \"x\" {\n  param p : float = 1 range 0..2 wibble\n}\n",
        "unexpected an identifier after the end of a statement",
        "statements end at a newline; put this on its own line",
    );
}

#[test]
fn an_unknown_type_lists_the_real_ones() {
    rejects(
        "lumen 1\neffect \"x\" {\n  param p : quaternion = 1\n}\n",
        "unknown type `quaternion`",
        "types are float, int, bool, angle, vec2, vec3, color, palette and curve",
    );
}

// ---- channel ---------------------------------------------------------------

#[test]
fn an_unknown_channel_type_lists_the_real_ones() {
    rejects(
        "lumen 1\neffect \"x\" {\n  channel c : telemetry\n}\n",
        "unknown channel type `telemetry`",
        "channel types are audio_bands, audio_beat, sim<..>, sensor<..>, value, vec3 and text",
    );
}

#[test]
fn every_channel_type_parses_to_its_own_node() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  channel a : audio_bands\n  channel b : audio_beat\n  channel v : value\n  channel w : vec3\n  channel s : sim<flock>\n  channel n : sensor<lux>\n  channel t : text\n  channel u : text(16)\n  layer l { color = rgb(v, v, v) }\n}\n",
    );
    let tys: Vec<&ChanType> = e.channels.iter().map(|c| &c.ty).collect();
    assert_eq!(
        tys,
        vec![
            &ChanType::AudioBands,
            &ChanType::AudioBeat,
            &ChanType::Value,
            &ChanType::Vec3,
            &ChanType::Sim("flock".into()),
            &ChanType::Sensor("lux".into()),
            // A bare `text` defaults to 64 bytes; the wire format needs a bound.
            &ChanType::Text(64),
            &ChanType::Text(16),
        ]
    );
}

#[test]
fn a_channel_keeps_its_hold_time_and_default() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  channel c : value hold 250 default 0.5\n  layer b { color = rgb(c, 0, 0) }\n}\n",
    );
    assert_eq!(e.channels[0].hold_ms, Some(250));
    assert!(e.channels[0].default.is_some());
}

#[test]
fn a_word_that_is_not_a_channel_modifier_ends_the_statement() {
    rejects(
        "lumen 1\neffect \"x\" {\n  channel c : value wibble\n}\n",
        "unexpected an identifier after the end of a statement",
        "statements end at a newline; put this on its own line",
    );
}

// ---- layer -----------------------------------------------------------------

#[test]
fn an_unknown_blend_mode_lists_the_real_ones() {
    rejects(
        "lumen 1\neffect \"x\" {\n  layer b blend sparkle { color = rgb(0,0,0) }\n}\n",
        "unknown blend mode `sparkle`",
        "blend modes are normal, add, multiply, screen, overlay, max, min and difference",
    );
}

#[test]
fn a_word_that_is_not_a_layer_modifier_is_refused_before_the_block() {
    // Not "expected an effect item": the parser is already inside `layer`, and
    // reporting the wrong context sends the author to the wrong line.
    rejects(
        "lumen 1\neffect \"x\" {\n  layer b wibble { color = rgb(0,0,0) }\n}\n",
        "expected `{`, found an identifier",
        "add `{` here",
    );
}

#[test]
fn an_unclosed_layer_block_is_reported_at_the_layer() {
    rejects(
        "lumen 1\neffect \"x\" {\n  layer b {\n    color = rgb(0,0,0)\n",
        "unclosed `layer` block",
        "add a closing `}`",
    );
}

#[test]
fn a_layer_body_holds_only_lets_and_assignments() {
    rejects(
        "lumen 1\neffect \"x\" {\n  layer b {\n    42\n  }\n}\n",
        "expected an assignment or `let`, found a number",
        "a layer contains `let` bindings and assignments like `color = ...`",
    );
}

#[test]
fn an_assignment_may_name_a_field() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  layer b {\n    color = rgb(0,0,0)\n    color.a = 0.5\n  }\n}\n",
    );
    let l = &e.layers[0];
    assert_eq!(l.assigns.len(), 2);
    assert_eq!(l.assigns[0].target, "color");
    assert_eq!(l.assigns[0].field, None);
    assert_eq!(l.assigns[1].field.as_deref(), Some("a"));
}

// ---- fn --------------------------------------------------------------------

#[test]
fn an_unclosed_fn_block_is_reported_at_the_fn() {
    rejects(
        "lumen 1\nfn f(a : float) -> float {\n  return a\n",
        "unclosed `fn` block",
        "add a closing `}`",
    );
}

#[test]
fn a_function_body_holds_only_lets_and_a_return() {
    rejects(
        "lumen 1\nfn f() -> float {\n  42\n}\n",
        "expected `let` or `return`, found a number",
        "a function body is a sequence of `let` bindings ending in `return`",
    );
}

#[test]
fn a_function_without_a_return_is_refused() {
    // A function that falls off the end has no value to inline, and inlining is
    // the only thing the compiler does with one.
    rejects(
        "lumen 1\nfn f() -> float {\n  let a = 1\n}\n",
        "function has no `return`",
        "a function body ends with `return <expression>`",
    );
}

#[test]
fn a_function_may_declare_no_parameters_and_no_return_type() {
    let ds = decls("lumen 1\nfn f() {\n  return 1\n}\n");
    match &ds[0] {
        Decl::Fn(f) => {
            assert_eq!(f.name, "f");
            assert!(f.params.is_empty());
            assert_eq!(f.ret, None);
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

#[test]
fn function_parameters_keep_their_declared_types() {
    let ds = decls("lumen 1\nfn f(a : float, b : vec3) -> color {\n  return rgb(a, 0, 0)\n}\n");
    match &ds[0] {
        Decl::Fn(f) => {
            assert_eq!(
                f.params,
                vec![("a".into(), Type::Float), ("b".into(), Type::Vec3)]
            );
            assert_eq!(f.ret, Some(Type::Color));
        }
        other => panic!("expected a fn, got {other:?}"),
    }
}

// ---- palette and curve -----------------------------------------------------

#[test]
fn an_unclosed_palette_block_is_reported_at_the_palette() {
    rejects(
        "lumen 1\npalette p {\n  0 #000000\n",
        "unclosed `palette` block",
        "add a closing `}`",
    );
}

#[test]
fn an_unknown_colour_space_lists_the_real_ones() {
    rejects(
        "lumen 1\npalette p {\n  space cielab\n  0 #000000\n}\n",
        "unknown colour space `cielab`",
        "colour spaces are oklab, oklch, hsv and linear_rgb",
    );
}

#[test]
fn a_palette_holds_only_a_space_and_stops() {
    rejects(
        "lumen 1\npalette p {\n  wibble\n}\n",
        "expected a stop position or `space`, found an identifier",
        "a palette contains an optional `space` and stops like `0.5 #ff8000`",
    );
}

#[test]
fn a_palette_keeps_its_declared_space() {
    use lumen_lang::ast::ColorSpace;
    let ds = decls("lumen 1\npalette p {\n  space hsv\n  0 #000000\n  1 #ffffff\n}\n");
    match &ds[0] {
        Decl::Palette(p) => {
            assert_eq!(p.space, ColorSpace::Hsv);
            assert_eq!(p.stops.len(), 2);
            assert_eq!(p.stops[0].position, 0.0);
            assert_eq!(p.stops[1].position, 1.0);
        }
        other => panic!("expected a palette, got {other:?}"),
    }
}

#[test]
fn a_curve_is_a_list_of_pairs() {
    let ds = decls("lumen 1\ncurve c {\n  0 0\n  0.5 0.8\n  1 1\n}\n");
    match &ds[0] {
        Decl::Curve(c) => {
            assert_eq!(c.name, "c");
            assert_eq!(c.points, vec![(0.0, 0.0), (0.5, 0.8), (1.0, 1.0)]);
        }
        other => panic!("expected a curve, got {other:?}"),
    }
}

#[test]
fn an_unclosed_curve_block_is_reported_at_the_curve() {
    rejects(
        "lumen 1\ncurve c {\n  0 0\n",
        "unclosed `curve` block",
        "add a closing `}`",
    );
}

#[test]
fn a_curve_point_must_start_with_a_number() {
    rejects(
        "lumen 1\ncurve c {\n  wibble 1\n}\n",
        "expected a number, found an identifier",
        "a curve is a list of `x y` pairs, one per line",
    );
}

#[test]
fn a_curve_point_needs_both_coordinates() {
    rejects(
        "lumen 1\ncurve c {\n  0.5\n}\n",
        "expected a second number, found end of line",
        "each curve line is an `x y` pair",
    );
}

#[test]
fn a_curve_recovers_and_reports_every_bad_line() {
    // One error per parse turns fixing a file into a slow game.
    let ds = diags("lumen 1\ncurve c {\n  wibble 1\n  0.5\n  1 1\n}\n");
    assert_eq!(ds.len(), 2, "{ds:?}");
    assert!(ds[0].0.starts_with("expected a number"), "{ds:?}");
    assert!(ds[1].0.starts_with("expected a second number"), "{ds:?}");
}

// ---- sim -------------------------------------------------------------------

#[test]
fn a_sim_takes_named_arguments_and_a_body() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  sim flock(count = 10, speed = 2) {\n    let a = 1\n    p = a\n  }\n  layer b { color = rgb(0,0,0) }\n}\n",
    );
    let s = &e.sims[0];
    assert_eq!(s.name, "flock");
    let names: Vec<&str> = s.args.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["count", "speed"]);
    assert!(matches!(s.body[0], SimStmt::Let(_)));
    assert!(matches!(s.body[1], SimStmt::Assign(_)));
}

#[test]
fn a_sim_if_may_have_no_else() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  sim s() {\n    if a > 1 {\n      b = 2\n    }\n  }\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.sims[0].body[0] {
        SimStmt::If {
            then, otherwise, ..
        } => {
            assert_eq!(then.len(), 1);
            assert!(otherwise.is_empty(), "a missing `else` is not an empty one");
        }
        other => panic!("expected an if, got {other:?}"),
    }
}

#[test]
fn a_sim_if_keeps_its_else_branch() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  sim s() {\n    if a > 1 {\n      b = 2\n    } else {\n      b = 3\n    }\n  }\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.sims[0].body[0] {
        SimStmt::If {
            then, otherwise, ..
        } => {
            assert_eq!(then.len(), 1);
            assert_eq!(otherwise.len(), 1);
        }
        other => panic!("expected an if, got {other:?}"),
    }
}

#[test]
fn a_sim_foreach_names_its_variable_and_its_array() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  sim s() {\n    foreach p in particles {\n      p = 1\n    }\n  }\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.sims[0].body[0] {
        SimStmt::ForEach {
            binding,
            over,
            body,
            ..
        } => {
            assert_eq!(binding, "p");
            assert_eq!(over, "particles");
            assert_eq!(body.len(), 1);
        }
        other => panic!("expected a foreach, got {other:?}"),
    }
}

#[test]
fn a_foreach_without_in_is_reported_but_still_parsed() {
    // Recovering here is what lets the rest of the sim be checked in the same
    // run rather than hiding behind one missing word.
    rejects(
        "lumen 1\neffect \"x\" {\n  sim s() {\n    foreach p particles {\n      p = 1\n    }\n  }\n  layer l { color = rgb(0,0,0) }\n}\n",
        "expected `in`",
        "write `foreach p in particles { ... }`",
    );
}

#[test]
fn a_sim_body_holds_only_statements() {
    rejects(
        "lumen 1\neffect \"x\" {\n  sim s() {\n    42\n  }\n  layer l { color = rgb(0,0,0) }\n}\n",
        "expected a statement, found a number",
        "a sim contains `let`, assignments, `if` and `foreach`",
    );
}

#[test]
fn an_unclosed_sim_block_is_reported() {
    rejects(
        "lumen 1\neffect \"x\" {\n  sim s() {\n    a = 1\n",
        "unclosed block",
        "add a closing `}`",
    );
}

// ---- expressions -----------------------------------------------------------

#[test]
fn an_expression_that_is_missing_entirely_is_reported() {
    rejects(
        "lumen 1\neffect \"x\" {\n  let a = \n}\n",
        "expected an expression, found end of line",
        "an expression is a number, a name, a call, or an operation on them",
    );
}

#[test]
fn every_comparison_and_logical_operator_parses() {
    use lumen_lang::ast::BinOp;
    let ops: &[(&str, BinOp)] = &[
        ("<", BinOp::Lt),
        ("<=", BinOp::Le),
        (">", BinOp::Gt),
        (">=", BinOp::Ge),
        ("==", BinOp::Eq),
        ("!=", BinOp::Ne),
        ("&&", BinOp::And),
        ("||", BinOp::Or),
    ];
    for (text, want) in ops {
        let src = alloc_format(text);
        let e = effect_of(&src);
        match &e.lets[0].value.kind {
            ExprKind::Binary { op, .. } => assert_eq!(op, want, "for `{text}`"),
            other => panic!("expected a binary node for `{text}`, got {other:?}"),
        }
    }
}

/// A one-`let` effect whose binding is `1 <op> 2`.
fn alloc_format(op: &str) -> String {
    format!(
        "lumen 1\neffect \"x\" {{\n  let a = 1 {op} 2\n  layer b {{ color = rgb(0,0,0) }}\n}}\n"
    )
}

#[test]
fn both_unary_operators_parse() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  let a = -1\n  let b = !(1 > 2)\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.lets[0].value.kind {
        ExprKind::Unary { op, .. } => assert_eq!(*op, UnOp::Neg),
        other => panic!("expected a unary node, got {other:?}"),
    }
    match &e.lets[1].value.kind {
        ExprKind::Unary { op, .. } => assert_eq!(*op, UnOp::Not),
        other => panic!("expected a unary node, got {other:?}"),
    }
}

#[test]
fn a_method_call_keeps_its_base_and_its_arguments() {
    // `flock.influence(p, r)` is a method call, not a field read of a call.
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  let a = flock.influence(1, 2)\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.lets[0].value.kind {
        ExprKind::MethodCall { base, method, args } => {
            assert_eq!(method, "influence");
            assert_eq!(args.len(), 2);
            assert!(matches!(&base.kind, ExprKind::Ident(n) if n == "flock"));
        }
        other => panic!("expected a method call, got {other:?}"),
    }
}

#[test]
fn a_field_read_is_not_a_method_call() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  let a = pos.x\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.lets[0].value.kind {
        ExprKind::Field { base, field } => {
            assert_eq!(field, "x");
            assert!(matches!(&base.kind, ExprKind::Ident(n) if n == "pos"));
        }
        other => panic!("expected a field, got {other:?}"),
    }
}

#[test]
fn a_field_name_must_be_a_name() {
    rejects(
        "lumen 1\neffect \"x\" {\n  let a = pos.5\n}\n",
        "expected a field name, found a number",
        "a field name must be a name like `my_field`",
    );
}

#[test]
fn a_call_with_no_arguments_parses() {
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  let a = rand()\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.lets[0].value.kind {
        ExprKind::Call { callee, args } => {
            assert_eq!(callee, "rand");
            assert!(args.is_empty());
        }
        other => panic!("expected a call, got {other:?}"),
    }
}

#[test]
fn a_string_is_an_expression() {
    // Not useful yet, but the grammar accepts it, and a node the parser builds
    // and nothing consumes is worse than one that is pinned.
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  let a = \"hello\"\n  layer l { color = rgb(0,0,0) }\n}\n",
    );
    match &e.lets[0].value.kind {
        ExprKind::Str(s) => assert_eq!(s, "hello"),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn milliseconds_and_percent_convert_at_parse_time() {
    // `90deg` is covered end to end; the other two conversions happen here and
    // nowhere else, so a wrong divisor would go unnoticed.
    let e = effect_of(
        "lumen 1\neffect \"x\" {\n  let a = 250ms\n  let b = 50%\n  layer l { color = rgb(a, b, 0) }\n}\n",
    );
    match &e.lets[0].value.kind {
        ExprKind::Number { value, unit } => {
            assert_eq!(*value, 0.25);
            assert_eq!(*unit, Some(Unit::Ms));
        }
        other => panic!("expected a number, got {other:?}"),
    }
    match &e.lets[1].value.kind {
        ExprKind::Number { value, unit } => {
            assert_eq!(*value, 0.5);
            assert_eq!(*unit, Some(Unit::Percent));
        }
        other => panic!("expected a number, got {other:?}"),
    }
}

// ---- the sRGB transfer function --------------------------------------------

#[test]
fn dark_colours_take_the_linear_leg_of_the_srgb_curve() {
    // Below 0.04045 sRGB is a straight line, not a power. Taking the power leg
    // there would crush the darkest few codes towards black.
    let e = effect_of("lumen 1\neffect \"x\" {\n  let a = #010101\n  layer l { color = a }\n}\n");
    match &e.lets[0].value.kind {
        ExprKind::Color(c) => {
            let want = (1.0 / 255.0) / 12.92;
            assert!((c[0] - want).abs() < 1e-12, "{c:?}");
            assert_eq!(c[3], 1.0);
        }
        other => panic!("expected a colour, got {other:?}"),
    }
}

#[test]
fn a_dark_colour_above_the_knee_still_takes_the_power_leg() {
    // Just above 0.04045 the exponent is strongly negative, which is the branch
    // of the local `exp` that divides rather than multiplies.
    let e = effect_of("lumen 1\neffect \"x\" {\n  let a = #0e0e0e\n  layer l { color = a }\n}\n");
    match &e.lets[0].value.kind {
        ExprKind::Color(c) => {
            // exp(2.4 * ln((14/255 + 0.055) / 1.055)), to six places.
            assert!((c[0] - 0.004_391_5).abs() < 1e-6, "{c:?}");
        }
        other => panic!("expected a colour, got {other:?}"),
    }
}

#[test]
fn the_srgb_transfer_is_anchored_at_both_ends() {
    use lumen_lang::parse::srgb_to_linear;
    assert_eq!(srgb_to_linear(0.0), 0.0);
    assert!((srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
    // Monotonic: a brighter code may never decode darker.
    let mut prev = -1.0;
    for i in 0..=255 {
        let v = srgb_to_linear(i as f64 / 255.0);
        assert!(v > prev, "not monotonic at {i}");
        prev = v;
    }
}

// ---- recovery --------------------------------------------------------------

#[test]
fn a_budget_that_is_not_a_number_claims_nothing() {
    let src =
        "lumen 1\neffect \"x\" {\n  budget lots on esp32c3\n  layer b { color = rgb(0,0,0) }\n}\n";
    rejects(
        src,
        "expected the budget, found an identifier",
        "the budget is a whole number",
    );
    // And no claim is recorded: a half-parsed budget that admission control
    // read as real would be worse than none.
    let (file, _) = parse(src);
    for d in file.expect("the parser still returns a tree").decls {
        if let Decl::Effect(e) = d {
            assert!(e.budgets.is_empty());
        }
    }
}

#[test]
fn recovery_stops_at_the_end_of_the_file_rather_than_looping() {
    // The recovery walk tracks brace depth, so an unclosed brace after a bad
    // statement leaves it looking for a newline it will never find. Running off
    // the end must terminate, not spin.
    let ds = diags("lumen 1\neffect \"x\" {\n  42 {\n");
    assert_eq!(ds[0].0, "expected an effect item, found a number");
    assert_eq!(
        ds[0].1,
        "inside an effect you can write `param`, `channel`, `let`, `mask`, `state`, `layer`, `sim` or `fn`"
    );
}
