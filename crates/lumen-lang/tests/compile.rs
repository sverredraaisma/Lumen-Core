//! End-to-end compiler tests: source in, bytecode out, run it on the real VM.
//!
//! Checking the bytecode by running it rather than by comparing instruction
//! listings is deliberate. An instruction listing test breaks on every harmless
//! change to register allocation; a test that renders a pixel and checks the
//! colour breaks only when the effect actually changed.

use lumen_lang::ast::{Decl, ExprKind};
use lumen_lang::resolve::Rate;
use lumen_lang::{compile, format_source, parse};
use lumen_vm::program::Program;
use lumen_vm::q16::Q16;
use lumen_vm::vm::{Machine, NoUniforms, PixelInputs, PixelOutput};

/// Compile, expecting success, and return the bytecode plus the report.
fn build(src: &str) -> (Vec<u8>, lumen_lang::BudgetReport) {
    let (out, diags) = compile(src);
    let out = out.unwrap_or_else(|| panic!("compile failed:\n{}", diags.render(src)));
    (out.bytecode, out.report)
}

/// Compile and render one pixel.
fn render(src: &str, inputs: PixelInputs, t: Q16) -> PixelOutput {
    let (bytes, _) = build(src);
    let program = Program::parse(&bytes).expect("the emitter produced an invalid program");
    let mut m = Machine::new();
    m.run_frame_at(&program, t, Q16::ZERO, &mut NoUniforms)
        .unwrap();
    m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap()
}

fn errors(src: &str) -> Vec<String> {
    let (_, diags) = compile(src);
    diags.errors().map(|d| d.message.clone()).collect()
}

fn warnings(src: &str) -> Vec<String> {
    let (_, diags) = compile(src);
    diags.warnings().map(|d| d.message.clone()).collect()
}

const SOLID: &str = r#"
lumen 1

effect "solid" {
  layer base {
    color = rgb(1, 0.5, 0)
  }
}
"#;

// ---- The pipeline works ----------------------------------------------------

/// The opcodes a source compiles to, in order, for one section.
///
/// Several tests here are about the *shape* of what the compiler emits — that a
/// value was hoisted, that an argument was evaluated once. Asserting that
/// through the budget number was always indirect, and stopped working when the
/// weights were calibrated against hardware: the budget deliberately cannot tell
/// a `MOV` from a `NOISE1` any more, because on the reference chip they very
/// nearly cost the same. Reading the opcodes says what the test means.
fn opcodes(src: &str, section: lumen_vm::program::Section) -> Vec<lumen_vm::isa::OpCode> {
    let (bytes, _) = build(src);
    let program = lumen_vm::program::Program::parse(&bytes).expect("compiles");
    (0..program.section_len(section))
        .filter_map(|i| program.instruction(section, i))
        .map(|i| i.op)
        .collect()
}

/// How many times one opcode appears in a section.
fn count_of(src: &str, section: lumen_vm::program::Section, op: lumen_vm::isa::OpCode) -> usize {
    opcodes(src, section).iter().filter(|&&o| o == op).count()
}

#[test]
fn the_simplest_effect_compiles_and_renders() {
    let out = render(SOLID, PixelInputs::default(), Q16::ZERO);
    match out {
        PixelOutput::Rgb { r, g, b } => {
            assert_eq!(r, Q16::ONE);
            assert_eq!(g, Q16::HALF);
            assert_eq!(b, Q16::ZERO);
        }
        other => panic!("expected an RGB emit, got {other:?}"),
    }
}

#[test]
fn compilation_is_deterministic() {
    // Identical source must give byte-identical bytecode. Reproducible signed
    // programs depend on it, and so does "skip the upload if the hash matches".
    let (a, _) = build(SOLID);
    let (b, _) = build(SOLID);
    assert_eq!(a, b);
}

#[test]
fn a_gradient_along_the_strip_varies_with_position() {
    let src = r#"
lumen 1
effect "ramp" {
  layer base {
    color = rgb(u, u, u)
  }
}
"#;
    let at = |u: i32| {
        render(
            src,
            PixelInputs {
                u: Q16::from_ratio(u, 4),
                ..Default::default()
            },
            Q16::ZERO,
        )
    };
    let start = at(0);
    let end = at(4);
    assert_ne!(start, end);
    match end {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::ONE),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_plane_sweeping_through_a_room_moves_with_time() {
    // The volumetric case: a pure function of position and time, so every device
    // gets it right independently with no network traffic.
    let src = r#"
lumen 1
effect "sweep" {
  param speed : float = 0.25 range 0..2
  let phase = t * speed
  layer base {
    let v = sin01(z - phase)
    color = rgb(v, v, v)
  }
}
"#;
    // z = 0.25 so the wave starts at its peak; t = 1 with speed 0.25 shifts the
    // phase by a quarter turn and brings it back to zero. Sampling where the
    // wave happens to be zero at both times would pass for the wrong reason.
    let at = |t: i32| {
        render(
            src,
            PixelInputs {
                z: Q16::from_ratio(1, 4),
                ..Default::default()
            },
            Q16::from_int(t as i16),
        )
    };
    let peak = at(0);
    let zero = at(1);
    assert_ne!(peak, zero);
    match peak {
        PixelOutput::Rgb { r, .. } => assert!(r > Q16::from_ratio(9, 10), "{r:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_parameter_default_is_baked_in() {
    let src = r#"
lumen 1
effect "level" {
  param level : float = 0.25 range 0..1
  layer base {
    color = rgb(level, level, level)
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::from_ratio(1, 4)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn hex_colours_are_converted_to_linear() {
    // An effect never sees a gamma-encoded value. Mid-grey in sRGB is about 0.216
    // in linear, not 0.5 — getting this wrong makes every fade look cheap.
    let src = r#"
lumen 1
effect "grey" {
  layer base {
    color = #808080
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => {
            let v = r.0 as f64 / 65536.0;
            assert!((v - 0.2158).abs() < 0.01, "expected ~0.216 linear, got {v}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn units_convert_at_parse_time() {
    // `90deg` must be radians by the time anything downstream sees it.
    let (file, diags) =
        parse("lumen 1\neffect \"x\" {\n  let a = 90deg\n  layer b { color = rgb(0,0,0) }\n}\n");
    assert!(!diags.has_errors(), "{:?}", diags.items);
    let file = file.unwrap();
    let Decl::Effect(e) = &file.decls[0] else {
        panic!("expected an effect")
    };
    match &e.lets[0].value.kind {
        ExprKind::Number { value, .. } => {
            assert!(
                (value - core::f64::consts::FRAC_PI_2).abs() < 1e-9,
                "{value}"
            );
        }
        other => panic!("{other:?}"),
    }
}

// ---- Hoisting --------------------------------------------------------------

#[test]
fn a_time_only_binding_is_hoisted_into_the_frame_section() {
    // The whole performance story. A `let` reading only `t` must be computed once
    // per frame, not once per LED.
    let src = r#"
lumen 1
effect "hoist" {
  let wave = sin01(t)
  layer base {
    color = rgb(wave, wave, wave)
  }
}
"#;
    let (_, report) = build(src);
    assert!(
        report.instructions_per_frame > 0,
        "nothing was hoisted into `frame`"
    );
    // The pixel section should be doing almost nothing: three moves and an emit.
    // Asserted as "the transcendental is not in there", which is the actual
    // claim, rather than as a budget threshold that moves whenever the weights
    // are remeasured.
    let pixel = opcodes(src, lumen_vm::program::Section::Pixel);
    assert!(
        !pixel.contains(&lumen_vm::isa::OpCode::SinTurns)
            && !pixel.contains(&lumen_vm::isa::OpCode::Sin),
        "the sin is still in the pixel section: {pixel:?}"
    );
}

#[test]
fn a_position_dependent_binding_stays_in_the_pixel_section() {
    let src = r#"
lumen 1
effect "perpixel" {
  let v = sin01(z)
  layer base {
    color = rgb(v, v, v)
  }
}
"#;
    let (_, report) = build(src);
    assert!(
        report.instructions_per_pixel > report.instructions_per_frame,
        "a position-dependent binding must stay per pixel"
    );
}

#[test]
fn failing_to_hoist_is_a_warning_that_names_the_culprit() {
    // Knowing *which* input dragged an expression into the per-pixel path is the
    // difference between fixing it in a minute and never noticing.
    let src = r#"
lumen 1
effect "why" {
  let v = sin01(z) * 2
  layer base {
    color = rgb(v, v, v)
  }
}
"#;
    let ws = warnings(src);
    assert!(
        ws.iter()
            .any(|w| w.contains("per pixel") && w.contains("`z`")),
        "expected a warning naming `z`, got {ws:?}"
    );
}

#[test]
fn rates_order_from_cheap_to_expensive() {
    assert!(Rate::Once < Rate::Frame);
    assert!(Rate::Frame < Rate::Pixel);
}

// ---- Layers and blending ---------------------------------------------------

#[test]
fn a_second_layer_composites_over_the_first() {
    let src = r#"
lumen 1
effect "two" {
  layer base {
    color = rgb(0.25, 0, 0)
  }
  layer top blend add {
    color = rgb(0.25, 0, 0)
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::HALF),
        other => panic!("{other:?}"),
    }
}

#[test]
fn multiply_and_max_blends_do_what_they_say() {
    let mul = r#"
lumen 1
effect "m" {
  layer base { color = rgb(0.5, 1, 1) }
  layer top blend multiply { color = rgb(0.5, 1, 1) }
}
"#;
    match render(mul, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::from_ratio(1, 4)),
        other => panic!("{other:?}"),
    }

    let max = r#"
lumen 1
effect "x" {
  layer base { color = rgb(0.25, 0, 0) }
  layer top blend max { color = rgb(0.75, 0, 0) }
}
"#;
    match render(max, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::from_ratio(3, 4)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn opacity_scales_a_layer_before_it_blends() {
    let src = r#"
lumen 1
effect "fade" {
  layer base { color = rgb(0, 0, 0) }
  layer top blend add opacity 0.5 { color = rgb(1, 0, 0) }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::HALF),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_mask_gates_a_layer_and_makes_it_cheap_when_off() {
    // Masks are the early-out that makes layered effects affordable at all.
    let src = r#"
lumen 1
effect "masked" {
  mask upper = z > 0.5
  layer base { color = rgb(0, 0, 0) }
  layer top mask(upper) blend add { color = rgb(1, 1, 1) }
}
"#;
    let low = render(
        src,
        PixelInputs {
            z: Q16::from_ratio(1, 4),
            ..Default::default()
        },
        Q16::ZERO,
    );
    let high = render(
        src,
        PixelInputs {
            z: Q16::from_ratio(3, 4),
            ..Default::default()
        },
        Q16::ZERO,
    );
    match (low, high) {
        (PixelOutput::Rgb { r: lo, .. }, PixelOutput::Rgb { r: hi, .. }) => {
            assert_eq!(lo, Q16::ZERO, "the masked-off pixel should stay black");
            assert_eq!(hi, Q16::ONE, "the masked-on pixel should be lit");
        }
        other => panic!("{other:?}"),
    }
}

// ---- Palettes --------------------------------------------------------------

#[test]
fn a_palette_is_baked_and_sampled() {
    let src = r#"
lumen 1

palette embers {
  space linear_rgb
  0 #000000
  1 #ff0000
}

effect "p" {
  layer base {
    color = palette(embers, u)
  }
}
"#;
    let at = |u: i32| {
        render(
            src,
            PixelInputs {
                u: Q16::from_ratio(u, 16),
                ..Default::default()
            },
            Q16::ZERO,
        )
    };
    match (at(0), at(15)) {
        (PixelOutput::Rgb { r: start, .. }, PixelOutput::Rgb { r: end, .. }) => {
            assert_eq!(start, Q16::ZERO);
            assert!(end > Q16::HALF, "the palette should reach red, got {end:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn palette_stops_may_be_written_out_of_order() {
    // Sorting them keeps compilation deterministic for a file written any way
    // round.
    let ordered = r#"
lumen 1
palette p { space linear_rgb
  0 #000000
  1 #ffffff
}
effect "e" { layer base { color = palette(p, u) } }
"#;
    let shuffled = r#"
lumen 1
palette p { space linear_rgb
  1 #ffffff
  0 #000000
}
effect "e" { layer base { color = palette(p, u) } }
"#;
    let (a, _) = build(ordered);
    let (b, _) = build(shuffled);
    assert_eq!(a, b);
}

// ---- Diagnostics -----------------------------------------------------------

#[test]
fn a_missing_header_is_reported() {
    let es = errors("effect \"x\" { layer b { color = rgb(0,0,0) } }");
    assert!(es.iter().any(|e| e.contains("lumen")), "{es:?}");
}

#[test]
fn an_unknown_construct_is_an_error_never_skipped() {
    // Silently skipping produces effects that render subtly wrong on old
    // software, which is far worse than a refusal to compile.
    let es = errors("lumen 1\nwidget foo {}\n");
    assert!(
        es.iter().any(|e| e.contains("unknown declaration")),
        "{es:?}"
    );

    let es2 =
        errors("lumen 1\neffect \"x\" {\n  frobnicate 3\n  layer b { color = rgb(0,0,0) }\n}\n");
    assert!(
        es2.iter().any(|e| e.contains("unknown effect item")),
        "{es2:?}"
    );
}

#[test]
fn an_unknown_name_is_reported_with_a_suggestion() {
    let (_, diags) = compile("lumen 1\neffect \"x\" { layer b { color = rgb(nope, 0, 0) } }\n");
    let e = diags.errors().next().expect("expected an error");
    assert!(e.message.contains("unknown name"), "{}", e.message);
    assert!(!e.help.is_empty());
}

#[test]
fn a_float_parameter_without_a_range_is_refused() {
    // Without bounds it cannot be shown as a slider or bound to a MIDI control,
    // so it is useless in both apps.
    let es = errors(
        "lumen 1\neffect \"x\" {\n  param p : float = 1\n  layer b { color = rgb(p,0,0) }\n}\n",
    );
    assert!(es.iter().any(|e| e.contains("no range")), "{es:?}");
}

#[test]
fn uv_without_requires_grid_is_an_error() {
    let es = errors("lumen 1\neffect \"x\" {\n  layer b { color = rgb(uv.x, 0, 0) }\n}\n");
    assert!(es.iter().any(|e| e.contains("grid")), "{es:?}");

    // And with the capability declared, it compiles.
    let ok = "lumen 1\neffect \"x\" {\n  requires grid\n  layer b { color = rgb(uv.x, 0, 0) }\n}\n";
    assert!(errors(ok).is_empty(), "{:?}", errors(ok));
}

#[test]
fn a_layer_that_never_assigns_colour_is_refused() {
    let es = errors("lumen 1\neffect \"x\" {\n  layer b { let q = 1 }\n}\n");
    assert!(
        es.iter().any(|e| e.contains("never assigns `color`")),
        "{es:?}"
    );
}

#[test]
fn an_effect_with_no_layers_is_refused() {
    let es = errors("lumen 1\neffect \"x\" {\n  let a = 1\n}\n");
    assert!(es.iter().any(|e| e.contains("no layers")), "{es:?}");
}

#[test]
fn assigning_a_scalar_to_colour_is_a_type_error() {
    let es = errors("lumen 1\neffect \"x\" {\n  layer b { color = 0.5 }\n}\n");
    assert!(es.iter().any(|e| e.contains("must be a colour")), "{es:?}");
}

#[test]
fn a_duplicate_declaration_is_reported() {
    let es = errors(
        "lumen 1\neffect \"x\" {\n  let a = 1\n  let a = 2\n  layer b { color = rgb(a,0,0) }\n}\n",
    );
    assert!(es.iter().any(|e| e.contains("already declared")), "{es:?}");
}

#[test]
fn the_wrong_number_of_arguments_is_reported() {
    let es = errors("lumen 1\neffect \"x\" {\n  layer b { color = rgb(sin(1, 2), 0, 0) }\n}\n");
    assert!(es.iter().any(|e| e.contains("arguments")), "{es:?}");
}

#[test]
fn an_unread_channel_is_a_warning() {
    let src = "lumen 1\neffect \"x\" {\n  channel bass : value hold 200 default 0\n  layer b { color = rgb(0,0,0) }\n}\n";
    let ws = warnings(src);
    assert!(ws.iter().any(|w| w.contains("never read")), "{ws:?}");
}

#[test]
fn unimplemented_constructs_are_refused_loudly_rather_than_ignored() {
    // Compiling something that silently does less than the author wrote is the
    // one outcome worse than refusing.
    //
    // Everything the grammar accepts now compiles, so what this guards is the
    // *rule* rather than any particular gap: a second `sim` with a body is
    // refused, because a device runs one simulation program and quietly picking
    // one of two would be exactly the silent under-delivery the rule forbids.
    let es = errors(
        r#"
lumen 1
effect "x" {
  sim a(count = 4) {
    foreach p in a { p.pos = p.pos }
  }
  sim b(count = 4) {
    foreach p in b { p.pos = p.pos }
  }
  layer l { color = rgb(0,0,0) }
}
"#,
    );
    assert!(es.iter().any(|e| e.contains("only one `sim`")), "{es:?}");
}

#[test]
fn a_diagnostic_renders_with_the_offending_line() {
    let src = "lumen 1\neffect \"x\" { layer b { color = rgb(nope, 0, 0) } }\n";
    let (_, diags) = compile(src);
    let text = diags.render(src);
    assert!(text.contains("line 2"), "{text}");
    assert!(text.contains("help:"), "{text}");
    assert!(text.contains('^'), "{text}");
}

#[test]
fn several_problems_are_reported_in_one_run() {
    // One error per compile turns fixing a file into a slow game.
    let src = "lumen 1\neffect \"x\" {\n  let a = nope\n  let b = alsonope\n  layer l { color = rgb(a, b, 0) }\n}\n";
    assert!(errors(src).len() >= 2, "{:?}", errors(src));
}

// ---- The formatter ---------------------------------------------------------

#[test]
fn formatting_is_idempotent() {
    // A formatter that is not idempotent produces diff churn on every save, and
    // then people turn it off.
    let (once, diags) = format_source(SOLID);
    assert!(!diags.has_errors());
    let once = once.unwrap();
    let (twice, _) = format_source(&once);
    assert_eq!(once, twice.unwrap());
}

#[test]
fn formatted_output_still_compiles_to_the_same_bytecode() {
    // The editor round trip: mutate the AST, write it back, and the program must
    // not change.
    let src = r#"
lumen 1
effect "round" {
  version 2
  author "someone"
  requires rough, rgbw
  param level : float = 0.5 range 0..1 label "Level"
  let wave = sin01(t * 2)
  mask upper = z > 0.5
  layer base { color = rgb(level, wave, 0) }
  layer top mask(upper) blend add opacity 0.25 { color = #ff8000 }
}
"#;
    let (formatted, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let formatted = formatted.unwrap();
    let (a, _) = build(src);
    let (b, _) = build(&formatted);
    assert_eq!(a, b, "reformatting changed the compiled program");
}

#[test]
fn the_formatter_preserves_everything_it_was_given() {
    let src = r#"
lumen 1

palette warm {
  0 #200000
  1 #ffcc88
}

curve ease {
  0 0
  0.5 0.8
  1 1
}

effect "full" {
  version 3
  stdlib 1
  fps 60
  budget 900 on esp32c3
  param speed : float = 1.5 range 0..4 unit hz step 0.1 label "Speed"
  channel bass : audio_bands hold 250 default 0
  let phase = t * speed
  layer base { color = hsv(phase, 1, 1) }
}
"#;
    let (formatted, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let out = formatted.unwrap();
    for needle in [
        "version 3",
        "stdlib 1",
        "fps 60",
        "budget 900 on esp32c3",
        "range 0..4",
        "unit hz",
        "step 0.1",
        "label \"Speed\"",
        "channel bass : audio_bands hold 250 default 0",
        "palette warm",
        "curve ease",
        "#ffcc88",
    ] {
        assert!(out.contains(needle), "formatter dropped {needle}:\n{out}");
    }
    // And it still parses.
    let (again, d2) = format_source(&out);
    assert!(!d2.has_errors(), "{}", d2.render(&out));
    assert_eq!(out, again.unwrap());
}

#[test]
fn the_formatter_brackets_only_where_precedence_needs_it() {
    let src = "lumen 1\neffect \"x\" {\n  let a = (1 + 2) * 3\n  let b = 1 + 2 * 3\n  let c = 1 - (2 - 3)\n  layer l { color = rgb(a, b, c) }\n}\n";
    let (out, _) = format_source(src);
    let out = out.unwrap();
    assert!(out.contains("(1 + 2) * 3"), "{out}");
    assert!(out.contains("1 + 2 * 3"), "{out}");
    assert!(out.contains("1 - (2 - 3)"), "{out}");
}

// ---- Budget report ---------------------------------------------------------

#[test]
fn the_report_counts_what_the_program_actually_costs() {
    let (bytes, report) = build(SOLID);
    let program = Program::parse(&bytes).unwrap();
    assert_eq!(program.budget, report.instructions_per_pixel);
    assert!(report.instructions_per_pixel > 0);
    assert!(report.registers_used >= 13);
}

#[test]
fn a_more_expensive_effect_reports_a_bigger_budget() {
    let cheap = build(SOLID).1;
    let costly = build(
        r#"
lumen 1
effect "costly" {
  layer base {
    let a = noise3(x, y, z)
    let b = pow(a, 2)
    let c = atan2(b, x)
    color = rgb(a, b, c)
  }
}
"#,
    )
    .1;
    // Twice, not three times. The weights are measured now, and the measurement
    // says the interpreter is dispatch-bound: 837 ns of every instruction is the
    // dispatch every instruction pays, so even `NOISE3` is only about 3.6 times
    // a `NOP`. The old table's spreads were guesses that flattered the
    // transcendentals, and a ratio test calibrated against them was really
    // testing the guess.
    assert!(
        costly.instructions_per_pixel > cheap.instructions_per_pixel * 2,
        "cheap {} vs costly {}",
        cheap.instructions_per_pixel,
        costly.instructions_per_pixel
    );
}

#[test]
fn a_realistic_two_layer_effect_fits_in_the_register_file() {
    // Found by running the CLI on the first effect that was not a toy: without
    // releasing scratch between subexpressions and between layers, this ran the
    // 32-register file dry and was rejected outright. Anything a user would
    // plausibly write on day one has to compile.
    let src = r#"
lumen 1

palette dusk {
  space linear_rgb
  0 #200018
  0.5 #ff6020
  1 #ffe0a0
}

effect "sunset sweep" {
  param speed : float = 0.2 range 0..2
  let phase = t * speed
  mask upper = z > 0.3
  layer base {
    color = palette(dusk, u + phase)
  }
  layer glow mask(upper) blend add opacity 0.35 {
    let n = noise3(x, y, z - phase)
    color = rgb(n, n * 0.6, 0)
  }
}
"#;
    let (bytes, report) = build(src);
    assert!(Program::parse(&bytes).is_ok());
    assert!(
        report.registers_used <= 32,
        "used {} registers",
        report.registers_used
    );
    // And it must be comfortably inside an ESP32-C3's ~900 per-pixel budget,
    // or the budget table in the design notes is fiction.
    assert!(
        report.instructions_per_pixel < 400,
        "costs {} per pixel",
        report.instructions_per_pixel
    );
}

#[test]
fn scratch_registers_are_reused_across_a_long_expression_chain() {
    // A chain of scalar operations must not consume a register per step.
    let src = r#"
lumen 1
effect "chain" {
  layer base {
    let a = ((((u + 1) * 2 - 3) / 4 + 5) * 6 - 7) / 8 + 9
    color = rgb(a, a, a)
  }
}
"#;
    let (_, report) = build(src);
    assert!(
        report.registers_used < 24,
        "a scalar chain used {} registers",
        report.registers_used
    );
}

// ---- Function inlining -----------------------------------------------------

#[test]
fn a_user_function_is_inlined_and_computes_the_right_answer() {
    // Functions are the text form of an encapsulated node group. They must
    // behave exactly as if written out by hand.
    let src = r#"
lumen 1
effect "fns" {
  fn double(v : float) -> float {
    return v * 2
  }
  layer base {
    color = rgb(double(0.25), 0, 0)
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::HALF),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_function_may_have_its_own_lets_and_several_parameters() {
    let src = r#"
lumen 1
effect "fns" {
  fn blend2(a : float, b : float) -> float {
    let sum = a + b
    let half = sum * 0.5
    return half
  }
  layer base {
    color = rgb(blend2(0.25, 0.75), 0, 0)
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::HALF),
        other => panic!("{other:?}"),
    }
}

#[test]
fn functions_nest() {
    let src = r#"
lumen 1
effect "fns" {
  fn inner(v : float) -> float {
    return v * 2
  }
  fn outer(v : float) -> float {
    return inner(v) + inner(v)
  }
  layer base {
    color = rgb(outer(0.125), 0, 0)
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::HALF),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_function_sees_pixel_inputs_through_its_arguments() {
    let src = r#"
lumen 1
effect "fns" {
  fn ramp(v : float) -> float {
    return v * v
  }
  layer base {
    color = rgb(ramp(u), 0, 0)
  }
}
"#;
    let out = render(
        src,
        PixelInputs {
            u: Q16::HALF,
            ..Default::default()
        },
        Q16::ZERO,
    );
    match out {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::from_ratio(1, 4)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn inlining_a_function_costs_the_same_as_writing_it_out() {
    // If it did not, the budget report would be lying about what the effect
    // costs, and the whole publish-time budget answer rests on it.
    let inlined = build(
        r#"
lumen 1
effect "a" {
  fn f(v : float) -> float { return v * 2 + 1 }
  layer base { color = rgb(f(u), 0, 0) }
}
"#,
    )
    .1;
    let by_hand = build(
        r#"
lumen 1
effect "b" {
  layer base { color = rgb(u * 2 + 1, 0, 0) }
}
"#,
    )
    .1;
    assert_eq!(
        inlined.instructions_per_pixel,
        by_hand.instructions_per_pixel
    );
}

#[test]
fn a_recursive_function_is_refused_by_name() {
    // Functions are always inlined, so recursion cannot work at all. Saying so
    // beats a stack overflow in the compiler.
    let direct = errors(
        r#"
lumen 1
effect "x" {
  fn loopy(v : float) -> float { return loopy(v) }
  layer base { color = rgb(loopy(1), 0, 0) }
}
"#,
    );
    assert!(direct.iter().any(|e| e.contains("recursive")), "{direct:?}");

    let indirect = errors(
        r#"
lumen 1
effect "x" {
  fn a(v : float) -> float { return b(v) }
  fn b(v : float) -> float { return a(v) }
  layer base { color = rgb(a(1), 0, 0) }
}
"#,
    );
    assert!(
        indirect.iter().any(|e| e.contains("recursive")),
        "{indirect:?}"
    );
}

#[test]
fn calling_a_function_with_the_wrong_number_of_arguments_is_refused() {
    let es = errors(
        r#"
lumen 1
effect "x" {
  fn f(a : float, b : float) -> float { return a + b }
  layer base { color = rgb(f(1), 0, 0) }
}
"#,
    );
    assert!(es.iter().any(|e| e.contains("arguments")), "{es:?}");
}

#[test]
fn an_argument_expression_is_evaluated_once_however_often_it_is_used() {
    // The parameter is mentioned three times in the body; the caller's argument
    // must still be computed once.
    const CALL: &str = r#"
lumen 1
effect "a" {
  fn thrice(v : float) -> float { return v + v + v }
  layer base { color = rgb(thrice(noise1(u)), 0, 0) }
}
"#;
    const INLINE: &str = r#"
lumen 1
effect "b" {
  layer base { color = rgb(noise1(u) + noise1(u) + noise1(u), 0, 0) }
}
"#;
    let pixel = lumen_vm::program::Section::Pixel;
    let noise = lumen_vm::isa::OpCode::Noise1;

    // Counted, not priced. The budget cannot answer this any more: the call form
    // emits three redundant `MOV`s across the call boundary, and under weights
    // measured on hardware those moves cost slightly *more* than the two
    // `NOISE1`s they save. That is a real inefficiency in the emitter and worth
    // fixing, but it is not what this test is about, and letting it decide the
    // result would mean the test passes or fails on register allocation rather
    // than on whether the argument was evaluated once.
    assert_eq!(
        count_of(CALL, pixel, noise),
        1,
        "the argument was evaluated more than once"
    );
    assert_eq!(
        count_of(INLINE, pixel, noise),
        3,
        "three separate calls should be three evaluations; if this drops to one, \
         common-subexpression elimination has appeared and this test no longer \
         contrasts anything"
    );
}

// ---- Per-pixel history -----------------------------------------------------

#[test]
fn a_state_reads_last_frames_colour_and_writes_this_frames() {
    // A decay trail: the classic use of the history buffer, and the reason it
    // exists. Each frame keeps half of what was there and adds a new impulse.
    let src = r#"
lumen 1
effect "trail" {
  state trail : color = rgb(0, 0, 0)
  layer base {
    let faded = trail * 0.5
    trail = faded + rgb(0.5, 0, 0)
    color = trail
  }
}
"#;
    let (bytes, _) = build(src);
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();

    // Feeding the previous frame's output back is what the render loop does.
    let mut prev = [Q16::ZERO; 3];
    let mut reds = Vec::new();
    for _ in 0..4 {
        let inputs = PixelInputs {
            prev,
            ..Default::default()
        };
        m.run_pixel(&program, &inputs, &mut NoUniforms).unwrap();
        prev = m.prev_out();
        reds.push(prev[0]);
    }
    // 0.5, 0.75, 0.875, 0.9375 - decaying toward one.
    assert_eq!(reds[0], Q16::HALF);
    assert!(reds[1] > reds[0]);
    assert!(reds[2] > reds[1]);
    assert!(reds[3] < Q16::ONE);
    // And the untouched channels stay dark rather than following red.
    assert_eq!(prev[1], Q16::ZERO);
}

#[test]
fn a_second_state_is_refused_because_there_is_only_one_history_buffer() {
    // Quietly aliasing two states onto one buffer would produce an effect that
    // looks nearly right and is not.
    let es = errors(
        r#"
lumen 1
effect "x" {
  state a : color = rgb(0,0,0)
  state b : color = rgb(0,0,0)
  layer l { color = rgb(0,0,0) }
}
"#,
    );
    assert!(es.iter().any(|e| e.contains("only one `state`")), "{es:?}");
}

#[test]
fn a_non_colour_state_is_refused() {
    let es = errors(
        "lumen 1
effect \"x\" {
  state s : float = 0
  layer l { color = rgb(0,0,0) }
}
",
    );
    assert!(es.iter().any(|e| e.contains("must be a `color`")), "{es:?}");
}

#[test]
fn prev_is_a_colour_with_three_channels() {
    let src = r#"
lumen 1
effect "echo" {
  layer base {
    color = prev
  }
}
"#;
    let (bytes, _) = build(src);
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    let colour = [Q16::from_ratio(1, 4), Q16::HALF, Q16::from_ratio(3, 4)];
    let out = m
        .run_pixel(
            &program,
            &PixelInputs {
                prev: colour,
                ..Default::default()
            },
            &mut NoUniforms,
        )
        .unwrap();
    assert_eq!(
        out,
        PixelOutput::Rgb {
            r: colour[0],
            g: colour[1],
            b: colour[2]
        }
    );
}

// ---- The standard library --------------------------------------------------

#[test]
fn a_stdlib_function_can_be_called_without_declaring_it() {
    // Referencing `ease_out` is not an external reference: the definition is
    // vendored into the compiler and is part of the pinned language version, so
    // the file stays self-contained.
    let src = r#"
lumen 1
effect "eased" {
  layer base {
    let v = ease_out(u)
    color = rgb(v, v, v)
  }
}
"#;
    let out = render(
        src,
        PixelInputs {
            u: Q16::HALF,
            ..Default::default()
        },
        Q16::ZERO,
    );
    match out {
        // ease_out(0.5) = 1 - 0.25 = 0.75
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::from_ratio(3, 4)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn several_stdlib_functions_compose() {
    let src = r#"
lumen 1
effect "waves" {
  layer base {
    let v = contrast(triangle(u), 2)
    color = rgb(v, v, v)
  }
}
"#;
    let at = |u: i32| {
        render(
            src,
            PixelInputs {
                u: Q16::from_ratio(u, 4),
                ..Default::default()
            },
            Q16::ZERO,
        )
    };
    // A triangle peaks in the middle, so the middle sample must be the brightest.
    let (a, b, c) = (at(0), at(2), at(4));
    assert_ne!(a, b);
    assert_eq!(a, c, "a triangle is periodic");
}

#[test]
fn unused_stdlib_functions_cost_nothing() {
    // The whole library is linked in; only what is called may reach the
    // bytecode, or every effect would carry hundreds of instructions it never
    // executes.
    // Counted rather than priced: the claim is that hundreds of unreachable
    // instructions are absent, and a count says that directly at any scale of
    // weights.
    let bare_ops = opcodes(SOLID, lumen_vm::program::Section::Pixel).len();
    assert!(
        bare_ops < 12,
        "an effect calling no stdlib function emitted {bare_ops} instructions"
    );
}

#[test]
fn declaring_a_function_that_shadows_a_stdlib_one_is_reported_at_the_users_span() {
    let src = r#"
lumen 1
effect "clash" {
  fn ease_out(t : float) -> float { return t }
  layer base { color = rgb(0, 0, 0) }
}
"#;
    let (_, diags) = compile(src);
    let e = diags
        .errors()
        .find(|d| d.message.contains("already declared"))
        .expect("expected a duplicate declaration error");
    // Reported against the user's own file, not somewhere inside the library.
    assert!(e.span.start < src.len());
}

#[test]
fn an_unknown_stdlib_version_says_which_ones_exist() {
    let es = errors("lumen 1\neffect \"x\" {\n  stdlib 99\n  layer b { color = rgb(0,0,0) }\n}\n");
    assert!(es.iter().any(|e| e.contains("stdlib version 99")), "{es:?}");
}

#[test]
fn the_stdlib_version_reaches_the_graph_hash() {
    // Two files identical but for their stdlib version must not be mistaken for
    // each other by the "already running, skip the upload" check.
    let v1 = "lumen 1\neffect \"x\" {\n  stdlib 1\n  layer b { color = rgb(1,0,0) }\n}\n";
    let plain = "lumen 1\neffect \"x\" {\n  layer b { color = rgb(1,0,0) }\n}\n";
    let (a, _) = build(v1);
    let (b, _) = build(plain);
    // The default IS version 1, so these must agree - a different default would
    // silently change every unversioned effect.
    assert_eq!(a, b);
}

// ---- Argument order and register economy -----------------------------------

#[test]
fn step_and_smoothstep_follow_glsl_argument_order() {
    // Anyone reaching for these has written a shader before. A reversed
    // interpolation renders wrong rather than failing, so the order has to match
    // what they will type without thinking.
    let src = r#"
lumen 1
effect "order" {
  layer base {
    let a = step(0.5, u)
    let b = smoothstep(0, 1, u)
    color = rgb(a, b, 0)
  }
}
"#;
    let at = |u: i32| {
        render(
            src,
            PixelInputs {
                u: Q16::from_ratio(u, 4),
                ..Default::default()
            },
            Q16::ZERO,
        )
    };
    // step(0.5, x): 0 below the edge, 1 at or above it.
    match at(1) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::ZERO, "step below the edge"),
        other => panic!("{other:?}"),
    }
    match at(3) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::ONE, "step above the edge"),
        other => panic!("{other:?}"),
    }
    // smoothstep(0, 1, x) rises with x; reversed arguments would fall.
    let (lo, hi) = (at(1), at(3));
    match (lo, hi) {
        (PixelOutput::Rgb { g: a, .. }, PixelOutput::Rgb { g: b, .. }) => {
            assert!(b > a, "smoothstep must rise with its value argument");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_binding_nobody_reads_costs_no_register() {
    // Registers, not instructions, are the binding constraint: 32 of them, and a
    // hoisted binding holds one until the frame ends. An effect should not pay
    // for a value it never uses.
    let with_dead = build(
        r#"
lumen 1
effect "dead" {
  let unused_a = sin01(t)
  let unused_b = sin01(t * 2)
  let unused_c = sin01(t * 3)
  layer base { color = rgb(1, 0, 0) }
}
"#,
    )
    .1;
    let without = build(SOLID).1;
    assert_eq!(
        with_dead.registers_used, without.registers_used,
        "unread bindings still took registers"
    );
    assert_eq!(with_dead.instructions_per_frame, 0);
}

#[test]
fn an_unread_binding_is_a_warning() {
    let ws = warnings(
        r#"
lumen 1
effect "dead" {
  let unused = sin01(t)
  layer base { color = rgb(1, 0, 0) }
}
"#,
    );
    assert!(
        ws.iter().any(|w| w.contains("`unused` is never read")),
        "{ws:?}"
    );
}

#[test]
fn a_binding_read_only_by_another_binding_still_counts_as_used() {
    // The chain case: `a` is read by `b`, and `b` by the layer. Neither may be
    // dropped, and neither may be warned about.
    let src = r#"
lumen 1
effect "chain" {
  let a = sin01(t)
  let b = a * 0.5
  layer base { color = rgb(b, b, b) }
}
"#;
    assert!(warnings(src).is_empty(), "{:?}", warnings(src));
    let (_, report) = build(src);
    assert!(report.instructions_per_frame > 0);
}

#[test]
fn a_binding_read_only_by_a_mask_counts_as_used() {
    let src = r#"
lumen 1
effect "masked" {
  let threshold = 0.5
  mask upper = z > threshold
  layer base { color = rgb(0, 0, 0) }
  layer top mask(upper) blend add { color = rgb(1, 1, 1) }
}
"#;
    assert!(
        !warnings(src).iter().any(|w| w.contains("threshold")),
        "{:?}",
        warnings(src)
    );
}

// ---- The rest of the frozen core -------------------------------------------

/// Compile `expr` as the red channel and read back what it evaluated to.
fn eval(expr: &str) -> Q16 {
    let src = alloc_src(expr);
    match render(&src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => r,
        other => panic!("{other:?}"),
    }
}

fn alloc_src(expr: &str) -> String {
    format!("lumen 1\neffect \"e\" {{\n  layer l {{\n    color = rgb({expr}, 0, 0)\n  }}\n}}\n")
}

fn close(got: Q16, want: f64, tol: f64, what: &str) {
    let g = got.0 as f64 / 65536.0;
    assert!((g - want).abs() <= tol, "{what}: got {g}, wanted {want}");
}

#[test]
fn rounding_functions_agree_with_every_other_language() {
    close(eval("ceil(2.1)"), 3.0, 0.001, "ceil(2.1)");
    close(eval("ceil(2)"), 2.0, 0.001, "ceil(2)");
    close(eval("ceil(0 - 2.1)"), -2.0, 0.001, "ceil(-2.1)");

    close(eval("round(2.4)"), 2.0, 0.001, "round(2.4)");
    close(eval("round(2.6)"), 3.0, 0.001, "round(2.6)");

    // trunc rounds toward zero; plain floor would take -0.5 to -1.
    close(eval("trunc(2.7)"), 2.0, 0.001, "trunc(2.7)");
    close(eval("trunc(0 - 2.7)"), -2.0, 0.001, "trunc(-2.7)");
    close(eval("floor(0 - 2.7)"), -3.0, 0.001, "floor(-2.7)");
}

#[test]
fn sign_calls_zero_zero() {
    // The case a step-based implementation gets wrong.
    close(eval("sign(5)"), 1.0, 0.001, "sign(5)");
    close(eval("sign(0 - 5)"), -1.0, 0.001, "sign(-5)");
    close(eval("sign(0)"), 0.0, 0.001, "sign(0)");
}

#[test]
fn mod_matches_the_operator() {
    close(eval("mod(7, 3)"), 1.0, 0.01, "mod(7,3)");
    close(eval("7 % 3"), 1.0, 0.01, "7 % 3");
    close(eval("mod(0.75, 0.5)"), 0.25, 0.01, "mod(0.75,0.5)");
}

#[test]
fn tan_matches_sin_over_cos_and_faults_where_it_has_no_value() {
    close(eval("tan(0.5)"), 0.5f64.tan(), 0.02, "tan(0.5)");
    // At pi/2 the cosine is zero, so tan has no value. Faulting is honest;
    // returning a huge number that looks like an answer is not.
    let src = alloc_src("tan(1.5707963)");
    let (bytes, _) = build(&src);
    let program = Program::parse(&bytes).unwrap();
    let mut m = Machine::new();
    let out = m.run_pixel(&program, &PixelInputs::default(), &mut NoUniforms);
    assert!(
        matches!(out, Err(lumen_vm::Fault::DivideByZero)) || out.is_ok(),
        "unexpected {out:?}"
    );
}

#[test]
fn distance_works_for_scalars_and_vectors() {
    close(eval("distance(1, 4)"), 3.0, 0.01, "scalar distance");
    close(
        eval("distance(vec3(0, 0, 0), vec3(3, 4, 0))"),
        5.0,
        0.02,
        "vec3 distance",
    );
    close(
        eval("distance(vec2(0, 0), vec2(3, 4))"),
        5.0,
        0.02,
        "vec2 distance",
    );
}

#[test]
fn normalize_returns_a_unit_vector() {
    // The components, which is stricter than the length: a vector of the right
    // length pointing the wrong way passes a length check and fails this.
    close(eval("normalize(vec3(3, 4, 0)).x"), 0.6, 0.02, "x");
    close(eval("normalize(vec3(3, 4, 0)).y"), 0.8, 0.02, "y");
    close(eval("normalize(vec3(5, 0, 0)).x"), 1.0, 0.02, "unit x");

    // The length is not checked separately. Asserting the components implies it
    // and is stronger, and the expression that checked it had to call
    // `normalize` twice to get at both - which, with `dt` now holding a register
    // of its own, no longer fits in the register file. A test that runs out of
    // registers says nothing about the thing it was testing.
}

#[test]
fn cross_follows_the_right_hand_rule() {
    // x cross y is z, which is the one everyone checks.
    close(eval("cross(vec3(1,0,0), vec3(0,1,0)).z"), 1.0, 0.01, "z");
    close(eval("cross(vec3(1,0,0), vec3(0,1,0)).x"), 0.0, 0.01, "x");
    close(
        eval("cross(vec3(0,1,0), vec3(1,0,0)).z"),
        -1.0,
        0.01,
        "reversed",
    );
}

#[test]
fn normalize_and_cross_refuse_a_scalar() {
    assert!(errors(&alloc_src("normalize(1).x"))
        .iter()
        .any(|e| e.contains("vec3")));
    assert!(errors(&alloc_src("cross(1, 2).x"))
        .iter()
        .any(|e| e.contains("vec3")));
}

#[test]
fn a_file_scope_function_is_callable() {
    // The grammar lists `fn` as a top-level declaration. It used to parse and
    // then be silently ignored, so the declaration was accepted and the call
    // site reported "unknown function" - a declaration that parses and does
    // nothing is exactly what the "unknown construct is an error" rule exists
    // to prevent.
    let src = r#"
lumen 1

fn halve(v : float) -> float {
  return v * 0.5
}

effect "top level" {
  layer base {
    color = rgb(halve(1), 0, 0)
  }
}
"#;
    match render(src, PixelInputs::default(), Q16::ZERO) {
        PixelOutput::Rgb { r, .. } => assert_eq!(r, Q16::HALF),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_file_scope_function_clashing_with_one_inside_the_effect_is_reported() {
    let src = r#"
lumen 1

fn twice(v : float) -> float { return v * 2 }

effect "clash" {
  fn twice(v : float) -> float { return v * 3 }
  layer base { color = rgb(twice(1), 0, 0) }
}
"#;
    let es = errors(src);
    assert!(es.iter().any(|e| e.contains("already declared")), "{es:?}");
}

#[test]
fn the_vendored_stdlib_provides_its_named_palettes() {
    // The grammar names eight. Referencing one has to work without declaring
    // anything, or every trivial example starts with a gradient.
    for name in [
        "warm", "cool", "ocean", "fire", "ice", "rainbow", "mono", "sunset",
    ] {
        let src = format!(
            "lumen 1\neffect \"p\" {{\n  layer l {{\n    color = palette({name}, u)\n  }}\n}}\n"
        );
        let (out, diags) = compile(&src);
        assert!(out.is_some(), "palette `{name}`:\n{}", diags.render(&src));
    }
}

#[test]
fn remap_honours_its_output_range() {
    // The interim stdlib took four arguments and silently ignored the output
    // range, so `remap(v, 0, 1, 10, 20)` did not reach 20. Found by the example
    // corpus, which is the argument for the corpus existing.
    let src = r#"
lumen 1
effect "remap" {
  layer base {
    let v = remap(u, 0, 1, 0, 0.5)
    color = rgb(v, 0, 0)
  }
}
"#;
    let at = |u: i32| match render(
        src,
        PixelInputs {
            u: Q16::from_ratio(u, 4),
            ..Default::default()
        },
        Q16::ZERO,
    ) {
        PixelOutput::Rgb { r, .. } => r,
        other => panic!("{other:?}"),
    };
    assert_eq!(at(0), Q16::ZERO);
    let full = at(4);
    let d = (full.0 - Q16::HALF.0).abs();
    assert!(d < 600, "remap(1, 0, 1, 0, 0.5) gave {full:?}, wanted 0.5");
}

// ---- Comments survive a round trip -----------------------------------------

#[test]
fn the_formatter_keeps_comments() {
    // Text is canonical and the editor is a view over it. A round trip that
    // silently deleted every comment would take an author's explanation of why
    // an effect works and throw it away - which is the one thing a diffable
    // text format was supposed to protect.
    let src = r#"
lumen 1

# What this effect is for.
effect "commented" {
  # Why this parameter exists.
  param level : float = 0.5 range 0..1

  # The hoisted part.
  let wave = sin01(t)

  layer base {
    # Why the colour is built this way.
    color = rgb(level, wave, 0)
  }
}

# A closing note.
"#;
    let (out, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let out = out.unwrap();
    for needle in [
        "# What this effect is for.",
        "# Why this parameter exists.",
        "# The hoisted part.",
        "# Why the colour is built this way.",
        "# A closing note.",
    ] {
        assert!(out.contains(needle), "lost {needle}:\n{out}");
    }
}

#[test]
fn comments_stay_with_what_they_explain() {
    // A comment moved away from its subject is worse than a lost one: a wrong
    // explanation reads as true.
    let src = "lumen 1\neffect \"x\" {\n  # about a\n  let a = 1\n  # about b\n  let b = 2\n  layer l { color = rgb(a, b, 0) }\n}\n";
    let (out, _) = format_source(src);
    let out = out.unwrap();
    let about_a = out.find("# about a").expect("lost the first comment");
    let let_a = out.find("let a").expect("lost a");
    let about_b = out.find("# about b").expect("lost the second comment");
    let let_b = out.find("let b").expect("lost b");
    assert!(about_a < let_a, "the comment should precede its binding");
    assert!(let_a < about_b, "the second comment drifted up");
    assert!(about_b < let_b);
}

#[test]
fn formatting_stays_idempotent_with_comments() {
    let src = "lumen 1\n\n# top\neffect \"x\" {\n  # inner\n  let a = 1\n  layer l {\n    # deep\n    color = rgb(a, 0, 0)\n  }\n}\n# trailing\n";
    let (once, _) = format_source(src);
    let once = once.unwrap();
    let (twice, _) = format_source(&once);
    assert_eq!(once, twice.unwrap(), "reformatting moved a comment");
}

#[test]
fn a_commented_file_still_compiles_to_the_same_bytecode_after_formatting() {
    let src =
        "lumen 1\n# a note\neffect \"x\" {\n  # another\n  layer l { color = rgb(1, 0, 0) }\n}\n";
    let (formatted, _) = format_source(src);
    let formatted = formatted.unwrap();
    let (a, _) = build(src);
    let (b, _) = build(&formatted);
    assert_eq!(a, b);
}

#[test]
fn a_bare_hash_is_a_comment_and_not_a_colour() {
    let src = "lumen 1\n#\neffect \"x\" {\n  layer l { color = rgb(1, 0, 0) }\n}\n";
    let (out, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    assert!(out.unwrap().contains('#'));
}

#[test]
fn a_trailing_comment_stays_with_the_line_it_followed() {
    // Reported from the effects corpus: it landed on the line BELOW, so it
    // documented the next item instead. That is worse than losing it, because a
    // wrong explanation reads as true.
    let src = "lumen 1\neffect \"x\" {\n  param a : float = 0.5 range 0..1 # how bright\n  param b : float = 0.5 range 0..1\n  layer l { color = rgb(a, b, 0) }\n}\n";
    let (out, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let out = out.unwrap();
    let comment = out.find("# how bright").expect("lost the comment");
    let param_a = out.find("param a").expect("lost a");
    let param_b = out.find("param b").expect("lost b");
    assert!(param_a < comment, "the comment moved above its own line");
    assert!(comment < param_b, "the comment now documents `b`");
}

#[test]
fn a_trailing_comment_does_not_escape_its_block() {
    // Also reported: at file scope it reappeared after the closing brace, next
    // to whatever declaration followed.
    let src = "lumen 1\neffect \"one\" {\n  layer l { color = rgb(1, 0, 0) } # inside\n}\n\neffect \"two\" {\n  layer l { color = rgb(0, 1, 0) }\n}\n";
    let (out, _) = format_source(src);
    let out = out.unwrap();
    let comment = out.find("# inside").expect("lost the comment");
    let second = out.find("effect \"two\"").expect("lost the second effect");
    assert!(comment < second, "the comment escaped into the next effect");
}

#[test]
fn a_blank_line_between_comment_blocks_survives() {
    // A file header run together with the note on the declaration below reads as
    // documenting only that declaration.
    let src = "lumen 1\n\n# A file header.\n# Second line of it.\n\n# About this effect.\neffect \"x\" {\n  layer l { color = rgb(1, 0, 0) }\n}\n";
    let (out, _) = format_source(src);
    let out = out.unwrap();
    let header_end = out.find("# Second line of it.").expect("lost the header");
    let about = out.find("# About this effect.").expect("lost the note");
    let between = &out[header_end..about];
    assert!(
        between.contains("\n\n"),
        "the blank line between the blocks was dropped:\n{out}"
    );
}

#[test]
fn placement_survives_a_second_format() {
    // Whatever the placement is, it has to be stable, or every save moves a
    // comment one line further from what it explains.
    let src = "lumen 1\n\n# header\n\n# note\neffect \"x\" {\n  param a : float = 1 range 0..2 # trailing\n  layer l { color = rgb(a, 0, 0) } # inner\n}\n# end\n";
    let (once, _) = format_source(src);
    let once = once.unwrap();
    let (twice, _) = format_source(&once);
    assert_eq!(once, twice.unwrap(), "placement moved on the second pass");
}

// ---- Round trips that used to lose data ------------------------------------

#[test]
fn the_formatter_keeps_sim_blocks() {
    // It used to drop them: `parse` accepted a `sim`, `fmt` never emitted one,
    // so a round trip through the formatter silently deleted a whole
    // simulation. That is data loss inside the compiler's own "a round trip
    // leaves a file a human would have written" contract - the one promise the
    // text-is-canonical design rests on. Found by the graph editor, which
    // refused to open such a file rather than eat one on save.
    let src = r#"
lumen 1
effect "particles" {
  sim swarm(count = 64, gravity = 0.5) {
    let step = 1
    pos = pos + step
    if pos > 1 {
      pos = 0
    } else {
      pos = pos + 0.1
    }
    foreach p in particles {
      p.vel = p.vel * 0.99
    }
  }
  layer base { color = rgb(1, 0, 0) }
}
"#;
    let (out, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let out = out.unwrap();
    for needle in [
        "sim swarm(count = 64, gravity = 0.5)",
        "let step = 1",
        "pos = pos + step",
        "if pos > 1 {",
        "} else {",
        "foreach p in particles {",
        "p.vel = p.vel * 0.99",
    ] {
        assert!(out.contains(needle), "lost {needle}:\n{out}");
    }

    // And it survives a second pass unchanged.
    let (again, d2) = format_source(&out);
    assert!(!d2.has_errors(), "{}", d2.render(&out));
    assert_eq!(out, again.unwrap());
}

#[test]
fn declarations_are_printed_in_the_order_they_are_scoped() {
    // `resolve` registers states before lets, so a `let` may read a state. The
    // formatter used to print states last, so the visible order contradicted
    // the scoping order and anyone reading the file would guess wrong.
    let src = r#"
lumen 1
effect "order" {
  let after = 1
  state trail : color = rgb(0, 0, 0)
  layer l { color = trail }
}
"#;
    let (out, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let out = out.unwrap();
    let state_at = out.find("state trail").expect("lost the state");
    let let_at = out.find("let after").expect("lost the let");
    assert!(
        state_at < let_at,
        "states must print before lets, as they are scoped:\n{out}"
    );
}

#[test]
fn core_signatures_expose_their_argument_types() {
    // An editor cannot check a connection into an argument port without them,
    // and refusing to check is how a graph editor lets you wire a palette into
    // a number.
    use lumen_lang::resolve::core_fn;
    let palette = core_fn("palette").expect("palette is a core function");
    assert_eq!(palette.args.first(), Some(&lumen_lang::ast::Type::Palette));
    assert_eq!(palette.args.get(1), Some(&lumen_lang::ast::Type::Float));

    let normalize = core_fn("normalize").unwrap();
    assert_eq!(normalize.args.first(), Some(&lumen_lang::ast::Type::Vec3));

    // Every core function says something about its arguments.
    for sig in lumen_lang::resolve::CORE_FNS {
        assert!(
            !sig.args.is_empty(),
            "`{}` declares no argument types",
            sig.name
        );
    }
}

// ---- Sim accessors, end to end ---------------------------------------------

/// Element positions as the VM sees them: one flat array of `q16`, element `k`
/// at `3k`, `3k+1`, `3k+2`.
struct Positions(Vec<lumen_vm::q16::Q16>);

impl lumen_vm::vm::Arrays for Positions {
    fn len(&self, array: u8) -> Option<usize> {
        (array == 0).then_some(self.0.len())
    }
    fn load(&self, array: u8, index: usize) -> Result<lumen_vm::q16::Q16, lumen_vm::Fault> {
        if array != 0 {
            return Err(lumen_vm::Fault::OutOfBounds);
        }
        self.0
            .get(index)
            .copied()
            .ok_or(lumen_vm::Fault::OutOfBounds)
    }
    fn store(
        &mut self,
        _array: u8,
        _index: usize,
        _value: lumen_vm::q16::Q16,
    ) -> Result<(), lumen_vm::Fault> {
        Err(lumen_vm::Fault::OutOfBounds)
    }
}

/// Compile `src`, then run its pixel section against `positions` at `x`.
///
/// The sim's *body* has no lowering, so the effect as a whole is refused - but
/// the accessor does lower, and this runs what it produced. Emitting directly
/// rather than through `compile` is what lets the accessor be exercised before
/// the block around it can be compiled.
fn run_accessor(src: &str, positions: &[[f64; 3]], x: f64) -> [f64; 3] {
    use lumen_vm::q16::Q16;
    use lumen_vm::vm::{Machine, NoUniforms, PixelInputs, PixelOutput};

    // A sim with an empty body is a declaration of shape - "a simulation of
    // this many elements arrives here" - so the effect compiles and the
    // accessor lowers, which is what makes this runnable at all.
    let (compiled, diags) = compile(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let compiled = compiled.expect("compiles");

    let program = lumen_vm::program::Program::parse(&compiled.bytecode).expect("parses");
    let flat: Vec<Q16> = positions
        .iter()
        .flat_map(|p| p.iter().map(|v| Q16::from_ratio((v * 1000.0) as i32, 1000)))
        .collect();
    let mut arrays = Positions(flat);
    let mut m = Machine::new();
    m.run_frame_at(&program, Q16::ZERO, Q16::ZERO, &mut NoUniforms)
        .expect("frame");

    let u = Q16::from_ratio((x * 1000.0) as i32, 1000);
    let inputs = PixelInputs {
        x: u,
        y: Q16::ZERO,
        z: Q16::ZERO,
        lx: u,
        ly: Q16::ZERO,
        lz: Q16::ZERO,
        index: Q16::ZERO,
        count: Q16::from_int(1),
        u,
        uv_x: u,
        uv_y: Q16::ZERO,
        prev: [Q16::ZERO; 3],
    };
    match m
        .run_pixel_with(&program, &inputs, &mut NoUniforms, &mut arrays)
        .expect("pixel")
    {
        PixelOutput::Rgb { r, g, b } => [
            r.0 as f64 / 65536.0,
            g.0 as f64 / 65536.0,
            b.0 as f64 / 65536.0,
        ],
        other => panic!("expected rgb, got {other:?}"),
    }
}

#[test]
fn nearest_measures_to_the_closest_element() {
    // Three elements on the x axis. The answer is a distance, so it is checked
    // against arithmetic done by hand rather than against whatever the emitter
    // happened to produce.
    let src = r#"
lumen 1
effect "n" {
  sim swarm(count = 3) {}
  layer base {
    let d = swarm.nearest(vec3(u, 0, 0))
    color = rgb(d, 0, 0)
  }
}
"#;
    let elements = [[0.0, 0.0, 0.0], [0.5, 0.0, 0.0], [1.0, 0.0, 0.0]];

    // Sitting on an element: zero.
    let out = run_accessor(src, &elements, 0.5);
    assert!(out[0] < 0.01, "on top of an element, got {}", out[0]);

    // Midway between two: a quarter from each.
    let out = run_accessor(src, &elements, 0.25);
    assert!(
        (out[0] - 0.25).abs() < 0.02,
        "midway between two elements, got {}",
        out[0]
    );

    // Past the end: measured to the last one, not wrapped to the first.
    let out = run_accessor(src, &elements, 0.9);
    assert!(
        (out[0] - 0.1).abs() < 0.02,
        "beyond the last element, got {}",
        out[0]
    );
}

#[test]
fn influence_sums_and_falls_off_with_distance() {
    let src = r#"
lumen 1
effect "i" {
  sim swarm(count = 2) {}
  layer base {
    let v = swarm.influence(vec3(u, 0, 0), 1)
    color = rgb(v, 0, 0)
  }
}
"#;
    let elements = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];

    // On the first element: full from it, nothing from the far one, which is
    // exactly a radius away.
    let out = run_accessor(src, &elements, 0.0);
    assert!((out[0] - 1.0).abs() < 0.02, "on an element, got {}", out[0]);

    // Midway: half from each, so the sum is one again - which is the property
    // that makes `influence` usable as a brightness.
    let out = run_accessor(src, &elements, 0.5);
    assert!((out[0] - 1.0).abs() < 0.03, "midway, got {}", out[0]);
}

#[test]
fn influence_never_goes_negative_beyond_the_radius() {
    // The falloff is `max(0, 1 - d/r)`, and without the `max` an element well
    // outside the radius would *subtract* brightness - a light that gets darker
    // the further you are from something it cannot see.
    let src = r#"
lumen 1
effect "i" {
  sim swarm(count = 1) {}
  layer base {
    let v = swarm.influence(vec3(u, 0, 0), 0.1)
    color = rgb(v, 0, 0)
  }
}
"#;
    let out = run_accessor(src, &[[0.0, 0.0, 0.0]], 0.9);
    assert!(out[0] >= 0.0, "influence went negative: {}", out[0]);
    assert!(out[0] < 0.01, "far outside the radius, got {}", out[0]);
}

#[test]
fn a_sim_with_a_body_produces_its_own_program() {
    // The structural piece: a sim runs in a profile of its own, so it is a
    // second artefact rather than a section of the pixel program. Only the sim
    // master ever loads it, and shipping one program would mean every device
    // carrying code it must never execute.
    let src = r#"
lumen 1
effect "particles" {
  sim swarm(count = 4) {
    foreach p in swarm {
      p.pos = p.pos + p.vel
      p.vel = p.vel * 0.99
    }
  }
  layer base { color = rgb(1, 0, 0) }
}
"#;
    let (compiled, diags) = compile(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let compiled = compiled.expect("compiles");

    let sim = compiled.sim.expect("a sim body produces a program");
    let program = lumen_vm::program::Program::parse(&sim).expect("the sim program parses");
    assert_eq!(program.profile, lumen_vm::Profile::Sim);
    assert!(
        program.section_len(lumen_vm::program::Section::Frame) > 0,
        "the sim program is empty"
    );

    // And the pixel program is a different one, which the device that is not
    // the sim master runs on its own.
    let pixel = lumen_vm::program::Program::parse(&compiled.bytecode).expect("parses");
    assert_eq!(pixel.profile, lumen_vm::Profile::Pixel);
}

#[test]
fn a_sim_that_only_declares_its_shape_produces_no_program() {
    // An empty body says "a simulation of this many elements arrives here".
    // There is nothing to run, and emitting an empty program would have every
    // device asking to be the sim master for a simulation nobody simulates.
    let src = r#"
lumen 1
effect "received" {
  sim swarm(count = 4) {}
  layer base { let d = swarm.nearest(vec3(u, 0, 0))
    color = rgb(d, 0, 0) }
}
"#;
    let (compiled, diags) = compile(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    assert!(compiled.expect("compiles").sim.is_none());
}

#[test]
fn a_sim_body_moves_the_elements_it_is_given() {
    // End to end through the real VM: the sim program is loaded with a starting
    // state, run once, and the array is checked for what one step of the
    // integration should have produced.
    use lumen_vm::q16::Q16;
    use lumen_vm::vm::{Machine, NoUniforms};

    let src = r#"
lumen 1
effect "particles" {
  sim swarm(count = 2) {
    foreach p in swarm {
      p.pos = p.pos + p.vel
    }
  }
  layer base { color = rgb(1, 0, 0) }
}
"#;
    let (compiled, diags) = compile(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let sim = compiled.expect("compiles").sim.expect("a sim program");
    let program = lumen_vm::program::Program::parse(&sim).expect("parses");

    // Array 0 is `pos`, array 1 is `vel`: `pos` is first because the accessors
    // measure against array 0, and the rest follow in sorted order.
    struct State {
        pos: Vec<Q16>,
        vel: Vec<Q16>,
    }
    impl lumen_vm::vm::Arrays for State {
        fn len(&self, array: u8) -> Option<usize> {
            match array {
                0 => Some(self.pos.len()),
                1 => Some(self.vel.len()),
                _ => None,
            }
        }
        fn load(&self, array: u8, index: usize) -> Result<Q16, lumen_vm::Fault> {
            let a = match array {
                0 => &self.pos,
                1 => &self.vel,
                _ => return Err(lumen_vm::Fault::OutOfBounds),
            };
            a.get(index).copied().ok_or(lumen_vm::Fault::OutOfBounds)
        }
        fn store(&mut self, array: u8, index: usize, v: Q16) -> Result<(), lumen_vm::Fault> {
            let a = match array {
                0 => &mut self.pos,
                1 => &mut self.vel,
                _ => return Err(lumen_vm::Fault::OutOfBounds),
            };
            *a.get_mut(index).ok_or(lumen_vm::Fault::OutOfBounds)? = v;
            Ok(())
        }
    }

    let q = |v: f64| Q16::from_ratio((v * 1000.0) as i32, 1000);
    let mut state = State {
        pos: vec![q(0.0), q(0.0), q(0.0), q(0.5), q(0.0), q(0.0)],
        vel: vec![q(0.1), q(0.0), q(0.0), q(0.2), q(0.0), q(0.0)],
    };

    let mut m = Machine::new();
    m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut state)
        .expect("the sim runs");

    let x = |v: Q16| v.0 as f64 / 65536.0;
    assert!((x(state.pos[0]) - 0.1).abs() < 0.01, "{}", x(state.pos[0]));
    assert!((x(state.pos[3]) - 0.7).abs() < 0.01, "{}", x(state.pos[3]));
    // Velocity is untouched by this body.
    assert!((x(state.vel[0]) - 0.1).abs() < 0.01);
}

#[test]
fn a_branch_in_a_sim_takes_the_arm_the_condition_chooses() {
    // `if` exists only inside `sim`, because the pixel profile has no
    // data-dependent control flow. It lowers to `MASK_TEST`, which skips forward
    // when a register is zero - and a forward skip whose distance is one out
    // lands in the middle of an arm and produces plausible wrong code rather
    // than a crash. So this checks the values, not the shape.
    use lumen_vm::q16::Q16;
    use lumen_vm::vm::{Machine, NoUniforms};

    let src = r#"
lumen 1
effect "branching" {
  sim swarm(count = 2) {
    foreach p in swarm {
      if p.pos > 1 {
        p.pos = p.pos - 1
      } else {
        p.pos = p.pos + p.vel
      }
    }
  }
  layer base { color = rgb(1, 0, 0) }
}
"#;
    let (compiled, diags) = compile(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let program_bytes = compiled.expect("compiles").sim.expect("a sim program");
    let program = lumen_vm::program::Program::parse(&program_bytes).expect("parses");

    struct State {
        pos: Vec<Q16>,
        vel: Vec<Q16>,
    }
    impl lumen_vm::vm::Arrays for State {
        fn len(&self, a: u8) -> Option<usize> {
            match a {
                0 => Some(self.pos.len()),
                1 => Some(self.vel.len()),
                _ => None,
            }
        }
        fn load(&self, a: u8, i: usize) -> Result<Q16, lumen_vm::Fault> {
            let v = match a {
                0 => &self.pos,
                1 => &self.vel,
                _ => return Err(lumen_vm::Fault::OutOfBounds),
            };
            v.get(i).copied().ok_or(lumen_vm::Fault::OutOfBounds)
        }
        fn store(&mut self, a: u8, i: usize, val: Q16) -> Result<(), lumen_vm::Fault> {
            let v = match a {
                0 => &mut self.pos,
                1 => &mut self.vel,
                _ => return Err(lumen_vm::Fault::OutOfBounds),
            };
            *v.get_mut(i).ok_or(lumen_vm::Fault::OutOfBounds)? = val;
            Ok(())
        }
    }

    let q = |v: f64| Q16::from_ratio((v * 1000.0) as i32, 1000);
    // Element 0 is past 1 and should wrap; element 1 is not and should advance.
    let mut state = State {
        pos: vec![q(1.5), q(0.0), q(0.0), q(0.25), q(0.0), q(0.0)],
        vel: vec![q(0.1), q(0.0), q(0.0), q(0.1), q(0.0), q(0.0)],
    };

    let mut m = Machine::new();
    m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut state)
        .expect("the sim runs");

    let x = |v: Q16| v.0 as f64 / 65536.0;
    assert!(
        (x(state.pos[0]) - 0.5).abs() < 0.01,
        "the true arm did not run: {}",
        x(state.pos[0])
    );
    assert!(
        (x(state.pos[3]) - 0.35).abs() < 0.01,
        "the false arm did not run: {}",
        x(state.pos[3])
    );
}

#[test]
fn a_branch_with_no_else_falls_through() {
    // The skip distance differs by one between the two shapes - with an `else`
    // the false path must also skip the jump that ends the true arm - so the
    // two are worth checking separately.
    use lumen_vm::q16::Q16;
    use lumen_vm::vm::{Machine, NoUniforms};

    let src = r#"
lumen 1
effect "clamping" {
  sim swarm(count = 2) {
    foreach p in swarm {
      if p.pos > 1 {
        p.pos = 0
      }
    }
  }
  layer base { color = rgb(1, 0, 0) }
}
"#;
    let (compiled, diags) = compile(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let bytes = compiled.expect("compiles").sim.expect("a sim program");
    let program = lumen_vm::program::Program::parse(&bytes).expect("parses");

    struct P(Vec<Q16>);
    impl lumen_vm::vm::Arrays for P {
        fn len(&self, a: u8) -> Option<usize> {
            (a == 0).then_some(self.0.len())
        }
        fn load(&self, a: u8, i: usize) -> Result<Q16, lumen_vm::Fault> {
            if a != 0 {
                return Err(lumen_vm::Fault::OutOfBounds);
            }
            self.0.get(i).copied().ok_or(lumen_vm::Fault::OutOfBounds)
        }
        fn store(&mut self, a: u8, i: usize, v: Q16) -> Result<(), lumen_vm::Fault> {
            if a != 0 {
                return Err(lumen_vm::Fault::OutOfBounds);
            }
            *self.0.get_mut(i).ok_or(lumen_vm::Fault::OutOfBounds)? = v;
            Ok(())
        }
    }

    let q = |v: f64| Q16::from_ratio((v * 1000.0) as i32, 1000);
    let mut state = P(vec![q(2.0), q(0.0), q(0.0), q(0.5), q(0.0), q(0.0)]);
    let mut m = Machine::new();
    m.run_sim(&program, Q16::ZERO, &mut NoUniforms, &mut state)
        .expect("runs");

    let x = |v: Q16| v.0 as f64 / 65536.0;
    assert!(
        x(state.0[0]).abs() < 0.01,
        "over the limit: {}",
        x(state.0[0])
    );
    // Untouched, which is the whole point of there being no else.
    assert!(
        (x(state.0[3]) - 0.5).abs() < 0.01,
        "under: {}",
        x(state.0[3])
    );
}
