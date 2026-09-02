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
    m.run_frame_at(&program, t, &mut NoUniforms).unwrap();
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
    assert!(
        report.instructions_per_pixel < 12,
        "pixel section costs {}, so the sin was not hoisted",
        report.instructions_per_pixel
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

palette fire {
  space linear_rgb
  0 #000000
  1 #ff0000
}

effect "p" {
  layer base {
    color = palette(fire, u)
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
    let es = errors(
        r#"
lumen 1
effect "x" {
  sim particles(count = 8) {
    a = 1
  }
  layer b { color = rgb(0,0,0) }
}
"#,
    );
    assert!(es.iter().any(|e| e.contains("not implemented")), "{es:?}");
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
    assert!(
        costly.instructions_per_pixel > cheap.instructions_per_pixel * 3,
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

palette sunset {
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
    color = palette(sunset, u + phase)
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
    let once = build(
        r#"
lumen 1
effect "a" {
  fn thrice(v : float) -> float { return v + v + v }
  layer base { color = rgb(thrice(noise1(u)), 0, 0) }
}
"#,
    )
    .1;
    let thrice = build(
        r#"
lumen 1
effect "b" {
  layer base { color = rgb(noise1(u) + noise1(u) + noise1(u), 0, 0) }
}
"#,
    )
    .1;
    assert!(
        once.instructions_per_pixel < thrice.instructions_per_pixel,
        "argument was evaluated more than once: {} vs {}",
        once.instructions_per_pixel,
        thrice.instructions_per_pixel
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
