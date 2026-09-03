//! Resolver tests: the rejections, the warnings, and the corners of the type
//! and rate analysis that a working effect never reaches.
//!
//! Every rejection is pinned by its exact message *and* its exact help line.
//! Diagnostics are a product surface here, and "type error" with no suggestion
//! is where an author stops.
//!
//! Most tests drive the whole `compile` pipeline, because that is what an author
//! runs. A few call `resolve` directly, where `compile` would short-circuit in an
//! earlier phase and the resolver's own check would never be reached.

use lumen_lang::ast::Type;
use lumen_lang::resolve::{self, Rate, SymbolKind};
use lumen_lang::{compile, parse, Diagnostics};

/// Every diagnostic from a full compile, as `(message, help)`.
fn diags(src: &str) -> Vec<(String, String)> {
    let (_, ds) = compile(src);
    ds.items
        .iter()
        .map(|d| (d.message.clone(), d.help.clone()))
        .collect()
}

/// Assert that some diagnostic has exactly this message, and that its help line
/// is exactly this too.
#[track_caller]
fn reports(src: &str, message: &str, help: &str) {
    let ds = diags(src);
    let found = ds
        .iter()
        .find(|(m, _)| m == message)
        .unwrap_or_else(|| panic!("no diagnostic said {message:?}; got {ds:?}"));
    assert_eq!(found.1, help, "help for {message:?}");
}

/// Resolve without linking the stdlib, for checks `compile` reaches earlier.
fn resolve_only(src: &str) -> Vec<(String, String)> {
    let (file, mut ds) = parse(src);
    let file = file.expect("source must parse");
    resolve::resolve(&file, &mut ds);
    ds.items
        .iter()
        .map(|d| (d.message.clone(), d.help.clone()))
        .collect()
}

/// A layer that assigns a colour, so an effect is otherwise well formed.
const BASE: &str = "  layer l { color = rgb(0,0,0) }\n";

fn effect(body: &str) -> String {
    format!("lumen 1\neffect \"x\" {{\n{body}{BASE}}}\n")
}

// ---- the file as a whole ---------------------------------------------------

#[test]
fn a_file_from_a_newer_language_is_refused_by_version() {
    // Compiling it anyway would produce bytecode for a program whose meaning
    // this compiler does not actually know.
    reports(
        "lumen 2\neffect \"x\" {\n  layer l { color = rgb(0,0,0) }\n}\n",
        "this compiler implements language version 1, but the file declares 2",
        "update the compiler, or change the `lumen` header if the file is newer than it needs to be",
    );
}

#[test]
fn a_file_with_no_effect_is_refused() {
    reports(
        "lumen 1\nfn f() -> float {\n  return 1\n}\n",
        "no `effect` declaration in this file",
        "a compilable file contains at least one `effect \"name\" { ... }`",
    );
}

#[test]
fn an_unknown_stdlib_version_is_reported_by_the_resolver_too() {
    // `compile` stops at the linker, which reports it first. The resolver keeps
    // its own check so a caller driving the phases separately still gets told.
    let ds =
        resolve_only("lumen 1\neffect \"x\" {\n  stdlib 99\n  layer l { color = rgb(0,0,0) }\n}\n");
    let found = ds
        .iter()
        .find(|(m, _)| m == "this compiler does not have stdlib version 99")
        .unwrap_or_else(|| panic!("{ds:?}"));
    // Built from what the compiler actually carries rather than written out.
    // Spelled as a literal this failed the moment a second version was added,
    // which is a test asserting the version list instead of the help text it is
    // about.
    let carried = lumen_lang::stdlib::available()
        .iter()
        .map(|v| v.0.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    assert_eq!(
        found.1,
        format!("it carries {carried}; update the compiler, or lower the `stdlib` line")
    );
}

#[test]
fn a_curve_is_declared_into_scope_under_its_own_name() {
    // A `curve` that parses but never reaches the symbol table is a declaration
    // that silently does nothing.
    let (file, mut ds) = parse("lumen 1\ncurve ease {\n  0 0\n  1 1\n}\neffect \"x\" {\n  layer l { color = rgb(0,0,0) }\n}\n");
    let file = file.expect("source must parse");
    let r = resolve::resolve(&file, &mut ds).expect("must resolve");
    let s = r.symbols.get("ease").expect("`ease` must be in scope");
    assert_eq!(s.kind, SymbolKind::Curve);
    assert_eq!(s.ty, Type::Curve);
    assert_eq!(s.rate, Rate::Once);
}

// ---- parameters and channels -----------------------------------------------

#[test]
fn a_default_without_the_parameters_unit_is_a_warning() {
    // `speed : float = 2 unit hz` reads as two hertz but is written as a bare
    // two; the mismatch is exactly the class of bug units exist to prevent.
    reports(
        &effect("  param speed : float = 2 range 0..10 unit hz\n  let a = speed\n"),
        "default for `speed` has no unit, but the parameter declares `hz`",
        "write the default as `…hz`",
    );
}

#[test]
fn a_default_that_carries_the_unit_is_not_warned_about() {
    let src = effect("  param speed : float = 2hz range 0..10 unit hz\n  let a = speed\n");
    let ds = diags(&src);
    assert!(!ds.iter().any(|(m, _)| m.contains("has no unit")), "{ds:?}");
}

#[test]
fn a_vec3_channel_resolves_as_a_vec3() {
    // Every other channel type reads as a float; getting this one wrong would
    // emit component-wise code against a scalar.
    let (file, mut ds) = parse(&effect("  channel wind : vec3\n  let a = wind.x\n"));
    let file = file.expect("source must parse");
    let r = resolve::resolve(&file, &mut ds).expect("must resolve");
    assert_eq!(r.symbols["wind"].ty, Type::Vec3);
    assert_eq!(r.symbols["wind"].rate, Rate::Frame);
}

// ---- masks -----------------------------------------------------------------

#[test]
fn a_layer_mask_that_names_nothing_is_refused() {
    reports(
        "lumen 1\neffect \"x\" {\n  layer l mask(nope) { color = rgb(0,0,0) }\n}\n",
        "unknown mask `nope`",
        "declare it with `mask name = <expression>` before the layer",
    );
}

#[test]
fn a_layer_mask_that_names_a_let_is_refused() {
    // A `let` used as a mask would be silently truthy-tested; the language says
    // masks are declared with `mask`.
    reports(
        "lumen 1\neffect \"x\" {\n  let m = 1\n  layer l mask(m) { color = rgb(0,0,0) }\n}\n",
        "`m` is not a mask",
        "a layer mask must name something declared with `mask`",
    );
}

// ---- expressions -----------------------------------------------------------

#[test]
fn a_string_in_an_expression_resolves_as_a_scalar() {
    // The grammar accepts one; the resolver must give it a type rather than
    // panicking on a node it did not expect.
    let (file, mut ds) = parse(&effect("  let a = \"hello\"\n  let b = a\n"));
    let file = file.expect("source must parse");
    let r = resolve::resolve(&file, &mut ds).expect("must resolve");
    assert_eq!(r.symbols["a"].ty, Type::Float);
    assert_eq!(r.symbols["a"].rate, Rate::Once);
}

#[test]
fn a_field_that_does_not_exist_on_the_type_is_refused() {
    reports(
        &effect("  let a = rgb(0,0,0).q\n"),
        "`color` has no field `q`",
        "vec2 has .x .y (or .u .v), vec3 has .x .y .z, color has .r .g .b .a",
    );
}

#[test]
fn a_field_read_on_a_scalar_is_refused() {
    reports(
        &effect("  let a = 1\n  let b = a.x\n"),
        "`float` has no field `x`",
        "vec2 has .x .y (or .u .v), vec3 has .x .y .z, color has .r .g .b .a",
    );
}

#[test]
fn an_unknown_function_is_refused_at_the_call_site() {
    reports(
        &effect("  let a = frobnicate(1)\n"),
        "unknown function `frobnicate`",
        "check the spelling, or declare it with `fn`",
    );
}

#[test]
fn an_overloaded_core_function_lists_both_arities() {
    // `length` takes 2 or 3; saying "takes 2" would send the author the wrong
    // way when they meant the 3-argument form.
    reports(
        &effect("  let a = length(1)\n"),
        "`length` takes 2 or 3 arguments, but 1 were given",
        "check the argument list",
    );
}

#[test]
fn mixing_two_vector_widths_is_refused_rather_than_emitted() {
    // Emitting this silently would produce code that reads past the narrower
    // operand: wrong pixels, no diagnostic.
    reports(
        &effect("  let a = vec2(1, 2) + vec3(1, 2, 3)\n"),
        "cannot apply `+` to `vec2` and `vec3`",
        "both sides must be the same width, or one of them a scalar",
    );
}

#[test]
fn a_vector_and_a_scalar_mix_freely() {
    let src = effect("  let a = vec3(1, 2, 3) * 2\n  let b = a.x\n");
    let ds = diags(&src);
    assert!(
        !ds.iter().any(|(m, _)| m.starts_with("cannot apply")),
        "{ds:?}"
    );
}

#[test]
fn not_yields_a_bool_whatever_it_is_applied_to() {
    let (file, mut ds) = parse(&effect(
        "  let a = !(1 > 2)\n  let b = -1\n  let c = a\n  let d = b\n",
    ));
    let file = file.expect("source must parse");
    let r = resolve::resolve(&file, &mut ds).expect("must resolve");
    assert_eq!(r.symbols["a"].ty, Type::Bool);
    // Negation keeps the operand's type; only `!` changes it.
    assert_eq!(r.symbols["b"].ty, Type::Float);
}

// ---- the hoisting warning names the culprit --------------------------------

/// The name the hoisting warning blamed for `let a = <expr>`, if it warned.
fn blamed(expr: &str) -> Option<String> {
    let src = effect(&format!(
        "  state s : color = rgb(0,0,0)\n  let a = {expr}\n  let keep = a\n"
    ));
    let (_, ds) = compile(&src);
    ds.items
        .iter()
        .find(|d| d.message.starts_with("`a` is computed per pixel"))
        .map(|d| d.message.clone())
}

#[test]
fn the_hoisting_warning_looks_through_every_kind_of_expression() {
    // Naming the culprit is the difference between an author fixing it in a
    // minute and never noticing, so every shape of expression must be walked.
    let cases: &[(&str, &str)] = &[
        // A pixel-rate builtin, read directly.
        ("u", "u"),
        // A pixel-rate symbol rather than a builtin: `state` is per-pixel.
        ("s.r", "s"),
        // Through a field, a call, a unary and a binary node.
        ("-u", "u"),
        ("sin(u) + 1", "u"),
        ("1 + u", "u"),
        // And through a method call's base.
        ("s.mix(1)", "s"),
    ];
    for (expr, culprit) in cases {
        let msg = blamed(expr).unwrap_or_else(|| panic!("no hoisting warning for `{expr}`"));
        assert_eq!(
            msg,
            format!("`a` is computed per pixel because it reads `{culprit}`"),
            "for `{expr}`"
        );
    }
}

#[test]
fn a_binding_that_reads_nothing_per_pixel_is_not_warned_about() {
    assert_eq!(blamed("1 + 2"), None);
    assert_eq!(blamed("t"), None);
}

// ---- recursion -------------------------------------------------------------

#[test]
fn a_function_that_calls_itself_from_an_argument_is_caught() {
    // The cycle hides inside an argument list rather than at the top of the
    // body; a walk that only looked at the callee would miss it.
    reports(
        "lumen 1\nfn f(a : float) -> float {\n  return sin(f(a))\n}\neffect \"x\" {\n  layer l { color = rgb(f(1),0,0) }\n}\n",
        "function `f` is recursive (through `f`)",
        "functions are always inlined, so they cannot recurse - rewrite it without the cycle",
    );
}

#[test]
fn a_cycle_through_another_functions_let_is_caught_and_named() {
    // `f -> g -> f`, where the call back to `f` is in `g`'s `let`, not `g`'s
    // return expression.
    reports(
        "lumen 1\nfn g(a : float) -> float {\n  let harmless = a * 2\n  let b = f(a) + harmless\n  return b\n}\nfn f(a : float) -> float {\n  return g(a)\n}\neffect \"x\" {\n  layer l { color = rgb(f(1),0,0) }\n}\n",
        "function `f` is recursive (through `g`)",
        "functions are always inlined, so they cannot recurse - rewrite it without the cycle",
    );
}

#[test]
fn a_cycle_reached_through_a_unary_or_a_method_call_is_caught() {
    reports(
        "lumen 1\nfn f(a : float) -> float {\n  return -f(a)\n}\neffect \"x\" {\n  layer l { color = rgb(f(1),0,0) }\n}\n",
        "function `f` is recursive (through `f`)",
        "functions are always inlined, so they cannot recurse - rewrite it without the cycle",
    );
    reports(
        "lumen 1\nfn f(a : float) -> float {\n  return a.mix(f(a))\n}\neffect \"x\" {\n  layer l { color = rgb(f(1),0,0) }\n}\n",
        "function `f` is recursive (through `f`)",
        "functions are always inlined, so they cannot recurse - rewrite it without the cycle",
    );
}

#[test]
fn a_cycle_in_a_functions_own_let_is_caught() {
    reports(
        "lumen 1\nfn f(a : float) -> float {\n  let b = f(a)\n  return b\n}\neffect \"x\" {\n  layer l { color = rgb(f(1),0,0) }\n}\n",
        "function `f` is recursive (through `f`)",
        "functions are always inlined, so they cannot recurse - rewrite it without the cycle",
    );
}

#[test]
fn a_deep_but_acyclic_call_chain_is_accepted() {
    // The recursion check is a depth-limited walk; it must not mistake a long
    // chain for a cycle.
    let src = "lumen 1\nfn a(x : float) -> float {\n  return b(x) + c(x)\n}\nfn b(x : float) -> float {\n  return c(x)\n}\nfn c(x : float) -> float {\n  return x * 2\n}\neffect \"x\" {\n  layer l { color = rgb(a(1),0,0) }\n}\n";
    let ds = diags(src);
    assert!(!ds.iter().any(|(m, _)| m.contains("recursive")), "{ds:?}");
}

// ---- duplicate declarations ------------------------------------------------

#[test]
fn a_name_declared_twice_points_at_the_earlier_one() {
    let ds = diags(&effect("  let a = 1\n  let a = 2\n"));
    let found = ds
        .iter()
        .find(|(m, _)| m == "`a` is already declared")
        .unwrap_or_else(|| panic!("{ds:?}"));
    assert!(
        found.1.starts_with("the earlier declaration is at byte "),
        "{found:?}"
    );
}

#[test]
fn unused_diagnostics_are_warnings_not_errors() {
    // A never-read binding must not stop a build; it is advice, not a refusal.
    let (out, ds) = compile(&effect("  let unread = 1\n"));
    assert!(out.is_some(), "{:?}", ds.items);
    assert_eq!(ds.errors().count(), 0);
    let w = ds
        .warnings()
        .find(|d| d.message == "`unread` is never read")
        .expect("expected the unread warning");
    assert_eq!(
        w.help,
        "remove it - registers are the scarce resource on this VM, and a hoisted binding holds one for the whole frame"
    );
}

/// Unused import guard: `Diagnostics` is part of the phase API these tests use.
#[test]
fn the_phases_can_be_driven_with_a_plain_diagnostics_sink() {
    let mut ds = Diagnostics::new();
    let (file, parse_ds) = parse("lumen 1\neffect \"x\" {\n  layer l { color = rgb(0,0,0) }\n}\n");
    ds.extend(parse_ds.items);
    let file = file.expect("must parse");
    assert!(resolve::resolve(&file, &mut ds).is_some());
    assert!(!ds.has_errors());
}

#[test]
fn the_hoisting_warning_skips_frame_rate_reads_and_names_the_pixel_one() {
    // `t` is frame rate and `u` is pixel rate. Blaming `t` would send the
    // author to rewrite the one part of the expression that was already cheap.
    assert_eq!(
        blamed("t + u"),
        Some("`a` is computed per pixel because it reads `u`".into())
    );
}
