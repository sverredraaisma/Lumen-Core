//! Formatter tests: the round trip has to leave a file a human would have
//! written, and it has to leave it alone the second time.
//!
//! Two things are being defended here. **Idempotence**, because a formatter that
//! keeps moving things produces diff churn on every save and then people turn it
//! off. And **nothing is lost**, because the node editor drives `ast` and `fmt`
//! directly: whatever the formatter drops, the editor deletes from the user's
//! file on save. It has previously dropped comments and whole `sim` blocks.

use lumen_lang::ast::{ChanType, Decl};
use lumen_lang::{format_source, parse};

/// Format once, asserting the source parsed cleanly.
fn fmt(src: &str) -> String {
    let (out, diags) = format_source(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    out.expect("the source parsed, so it must format")
}

/// Format, reformat, and assert the second pass changed nothing.
fn fmt_stable(src: &str) -> String {
    let once = fmt(src);
    let twice = fmt(&once);
    assert_eq!(once, twice, "formatting is not idempotent");
    once
}

/// Every `#` comment body in a source file, in order.
fn comment_texts(src: &str) -> Vec<String> {
    let (file, diags) = parse(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    file.expect("parsed")
        .comments
        .iter()
        .map(|c| c.text.clone())
        .collect()
}

// ---- The corpus ------------------------------------------------------------

/// Sources the round-trip properties are checked against.
///
/// Hand-written rather than read from the shipped examples in `lumen-effects`.
/// That sibling is not checked out in this repo's CI — `lumen-core` is
/// deliberately self-contained — and the corpus is already format-checked where
/// it lives, by `lumen-effects` CI running the real `lumen fmt --check` over
/// every example.
///
/// Writing them here is also strictly stronger for what these tests assert. The
/// shipped examples contain no `sim` block and no `curve`, so counting those on
/// both sides of a format would have compared zero with zero — and a dropped
/// `sim` is the exact regression this file exists to catch.
const CORPUS: &[(&str, &str)] = &[
    ("every-declaration", EVERY_DECLARATION),
    ("comments-everywhere", COMMENTS_EVERYWHERE),
    ("minimal", MINIMAL),
];

/// One of every declaration the formatter has to write back out.
const EVERY_DECLARATION: &str = r#"lumen 1

palette warm {
  space linear_rgb
  0 #2fd07a
  1 #d1443c
}

curve ease {
  0 0
  0.5 0.8
  1 1
}

fn falloff(d : float, k : float) -> float {
  let s = d * k
  return 1 - clamp(s, 0, 1)
}

effect "Every Declaration" {
  version 1
  author "lumen-core tests"
  stdlib 1
  requires grid
  fps 60

  param speed : float = 0.15 range 0.02..1 label "Speed"
  param tint : color = #204080 label "Tint"

  channel bass : value hold 400 default 0

  state heat : color = rgb(0, 0, 0)

  let phase = sine01(t * speed)
  let energy = clamp(bass, 0, 1)

  mask upper = z > 0.5

  fn boost(v : float) -> float {
    return v * v
  }

  sim embers(count = 64, gravity = 0.2) {
    let drift = gravity * dt
    foreach e in embers {
      e.y = e.y - drift
      if e.y < 0 {
        e.y = 1
      } else {
        e.life = e.life - dt
      }
    }
  }

  layer base {
    let pos = u * phase
    color = palette(warm, pos) * boost(energy)
  }

  layer top mask(upper) blend add {
    heat = prev * 0.9
    color = tint * falloff(v, 2)
  }
}
"#;

/// Comments in every position the lexer distinguishes.
const COMMENTS_EVERYWHERE: &str = r#"lumen 1

# A comment before the effect.

effect "Comments" {
  version 1
  author "lumen-core tests"
  stdlib 1
  fps 60

  # An own-line comment with a blank line before it.
  param speed : float = 0.15 range 0.02..1 label "Speed"  # and a trailing one

  #
  let phase = sine01(t * speed)

  layer base {
    # Inside a layer.
    color = rgb(phase, 0, 0)  # trailing, inside a layer
  }
}
"#;

/// The smallest thing that is still an effect.
const MINIMAL: &str = r#"lumen 1

effect "Minimal" {
  version 1
  author "lumen-core tests"
  stdlib 1
  fps 60

  layer base {
    color = rgb(0, 0, 0)
  }
}
"#;

/// Every fixture must parse, or the properties below are vacuous.
#[test]
fn every_fixture_in_the_corpus_parses() {
    for (name, src) in CORPUS {
        let (file, diags) = parse(src);
        assert!(
            !diags.has_errors(),
            "{name}:
{}",
            diags.render(src)
        );
        assert!(file.is_some(), "{name} did not parse");
    }
}

/// And between them they must actually contain the declarations the properties
/// below count, or "kept every declaration" compares zero with zero.
#[test]
fn the_corpus_exercises_every_declaration_kind() {
    let (file, _) = parse(EVERY_DECLARATION);
    let file = file.expect("parsed");
    assert!(file.decls.len() >= 4, "palette, curve, fn and effect");
    let effect = file
        .decls
        .iter()
        .find_map(|d| match d {
            Decl::Effect(e) => Some(e),
            _ => None,
        })
        .expect("an effect");
    assert!(!effect.params.is_empty(), "params");
    assert!(!effect.channels.is_empty(), "channels");
    assert!(!effect.states.is_empty(), "states");
    assert!(!effect.lets.is_empty(), "lets");
    assert!(!effect.masks.is_empty(), "masks");
    assert!(!effect.fns.is_empty(), "fns");
    assert!(
        !effect.sims.is_empty(),
        "sims - the dropped-sim regression guard"
    );
    assert!(effect.layers.len() >= 2, "layers");
    assert!(
        !comment_texts(COMMENTS_EVERYWHERE).is_empty(),
        "the comment fixture must contain comments"
    );
}

#[test]
fn every_example_formats_idempotently() {
    // parse -> fmt -> parse -> fmt, and the two outputs must be identical.
    // Anything that moves on the second pass moves on every save.
    for (name, src) in CORPUS {
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice, "{name} is not idempotent under formatting");
    }
}

#[test]
fn formatting_an_example_loses_no_comment() {
    // The formatter has deleted comments outright before. A comment is the one
    // thing in the file the compiler does not need and the author cannot
    // recover, so it is checked by text, not by count.
    for (name, src) in CORPUS {
        let before = comment_texts(src);
        let after = comment_texts(&fmt(src));
        assert_eq!(
            before, after,
            "{name} lost or reordered a comment when formatted"
        );
    }
}

#[test]
fn formatting_an_example_keeps_every_declaration() {
    // A `sim` block used to vanish entirely: `parse` accepted it, `fmt` never
    // wrote one out. Count the declarations and the effect's items on both
    // sides so a silently dropped block cannot pass.
    for (name, src) in CORPUS {
        let (before, _) = parse(src);
        let (after, _) = parse(&fmt(src));
        let (before, after) = (before.expect(name), after.expect(name));
        assert_eq!(
            before.decls.len(),
            after.decls.len(),
            "{name} lost a declaration"
        );
        for (b, a) in before.decls.iter().zip(&after.decls) {
            let (Decl::Effect(b), Decl::Effect(a)) = (b, a) else {
                continue;
            };
            assert_eq!(b.params.len(), a.params.len(), "{name} lost a param");
            assert_eq!(b.channels.len(), a.channels.len(), "{name} lost a channel");
            assert_eq!(b.states.len(), a.states.len(), "{name} lost a state");
            assert_eq!(b.lets.len(), a.lets.len(), "{name} lost a let");
            assert_eq!(b.masks.len(), a.masks.len(), "{name} lost a mask");
            assert_eq!(b.fns.len(), a.fns.len(), "{name} lost a function");
            assert_eq!(b.sims.len(), a.sims.len(), "{name} lost a sim block");
            assert_eq!(b.layers.len(), a.layers.len(), "{name} lost a layer");
        }
    }
}

// ---- Declarations ----------------------------------------------------------

#[test]
fn a_file_scope_function_is_printed_with_its_lets_and_return_type() {
    let src = r#"
lumen 1
fn ease(v : float) -> float {
  let k = v * v
  return k * (3 - 2 * v)
}
effect "x" {
  layer base { color = rgb(ease(u), 0, 0) }
}
"#;
    let out = fmt_stable(src);
    assert!(out.contains("fn ease(v : float) -> float {\n"), "{out}");
    assert!(out.contains("  let k = v * v\n"), "{out}");
    assert!(out.contains("  return k * (3 - 2 * v)\n"), "{out}");
}

#[test]
fn a_function_inside_an_effect_is_indented_one_level() {
    // The same printer runs at two depths; a function nested inside the effect
    // has to come out inside the braces, not flush against the margin.
    let src = r#"
lumen 1
effect "x" {
  fn twice(v : float) -> float {
    return v * 2
  }
  layer base { color = rgb(twice(u), 0, 0) }
}
"#;
    let out = fmt_stable(src);
    assert!(out.contains("  fn twice(v : float) -> float {\n"), "{out}");
    assert!(out.contains("    return v * 2\n"), "{out}");
    assert!(out.contains("  }\n"), "{out}");
}

#[test]
fn a_function_without_a_declared_return_type_omits_the_arrow() {
    let src = "lumen 1\nfn half(v : float) {\n  return v * 0.5\n}\neffect \"x\" {\n  layer base { color = rgb(half(u), 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("fn half(v : float) {\n"), "{out}");
    assert!(!out.contains("->"), "{out}");
}

#[test]
fn a_layer_keeps_its_own_lets_and_a_field_assignment() {
    let src = r#"
lumen 1
effect "x" {
  state heat : color = rgb(0, 0, 0)
  layer base {
    let v = u * 2
    heat.r = v
    color = rgb(v, 0, 0)
  }
}
"#;
    let out = fmt_stable(src);
    assert!(out.contains("    let v = u * 2\n"), "{out}");
    assert!(out.contains("    heat.r = v\n"), "{out}");
    assert!(out.contains("    color = rgb(v, 0, 0)\n"), "{out}");
}

#[test]
fn a_palette_prints_its_space_only_when_it_is_not_the_default() {
    let oklab = fmt_stable("lumen 1\npalette p {\n  0 #000000\n  1 #ffffff\n}\neffect \"x\" {\n  layer l { color = palette(p, u) }\n}\n");
    assert!(!oklab.contains("space"), "oklab is the default:\n{oklab}");

    let linear = fmt_stable("lumen 1\npalette p {\n  space linear_rgb\n  0 #000000\n  1 #ffffff\n}\neffect \"x\" {\n  layer l { color = palette(p, u) }\n}\n");
    assert!(linear.contains("  space linear_rgb\n"), "{linear}");
}

#[test]
fn every_channel_type_round_trips_through_the_formatter() {
    // The editor writes these back; a type it cannot print is a channel the
    // editor silently retypes on save.
    let src = r#"
lumen 1
effect "x" {
  channel a : audio_bands
  channel b : audio_beat
  channel c : sim<swarm>
  channel d : sensor<lux>
  channel e : value
  channel f : vec3
  channel g : text
  channel h : text(128)
  layer l { color = rgb(1, 0, 0) }
}
"#;
    let out = fmt_stable(src);
    for needle in [
        "channel a : audio_bands\n",
        "channel b : audio_beat\n",
        "channel c : sim<swarm>\n",
        "channel d : sensor<lux>\n",
        "channel e : value\n",
        "channel f : vec3\n",
        "channel g : text\n",
        "channel h : text(128)\n",
    ] {
        assert!(out.contains(needle), "formatter dropped {needle:?}:\n{out}");
    }
    // A bare `text` is `text(64)`, and must print back as the bare form rather
    // than ratcheting to `text(64)` on the first save.
    let (file, _) = parse(&out);
    let Some(Decl::Effect(e)) = file.unwrap().decls.into_iter().next() else {
        panic!("expected an effect");
    };
    assert_eq!(e.channels[6].ty, ChanType::Text(64));
    assert_eq!(e.channels[7].ty, ChanType::Text(128));
}

#[test]
fn a_channel_keeps_its_hold_and_default() {
    let src = "lumen 1\neffect \"x\" {\n  channel bass : audio_bands hold 250 default 0.5\n  layer l { color = rgb(bass, 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(
        out.contains("channel bass : audio_bands hold 250 default 0.5\n"),
        "{out}"
    );
}

// ---- Expressions -----------------------------------------------------------

#[test]
fn a_unit_that_survives_parsing_is_printed_back_with_its_suffix() {
    // Degrees, milliseconds and percent are *converted* at parse time, so
    // reprinting the suffix would be a lie about the value. Metres, seconds,
    // radians and hertz are not converted, so the suffix stays.
    let src = r#"
lumen 1
effect "x" {
  let a = 2m
  let b = 3s
  let c = 4rad
  let d = 5hz
  let e = 90deg
  let f = 250ms
  let g = 50%
  layer l { color = rgb(a + b + c + d + e + f + g, 0, 0) }
}
"#;
    let out = fmt_stable(src);
    assert!(out.contains("let a = 2m\n"), "{out}");
    assert!(out.contains("let b = 3s\n"), "{out}");
    assert!(out.contains("let c = 4rad\n"), "{out}");
    assert!(out.contains("let d = 5hz\n"), "{out}");
    // 90 degrees is pi/2 radians, 250ms is 0.25s, 50% is 0.5 - printed as the
    // value they became, with no suffix to contradict it.
    assert!(out.contains("let e = 1.570796\n"), "{out}");
    assert!(out.contains("let f = 0.25\n"), "{out}");
    assert!(out.contains("let g = 0.5\n"), "{out}");
}

#[test]
fn unary_operators_print_tight_against_their_operand() {
    let src = "lumen 1\neffect \"x\" {\n  let a = -u\n  let b = !a\n  let c = -(u + 1)\n  layer l { color = rgb(a, b, c) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("let a = -u\n"), "{out}");
    assert!(out.contains("let b = !a\n"), "{out}");
    // The operand binds tighter than any binary operator, so the brackets stay.
    assert!(out.contains("let c = -(u + 1)\n"), "{out}");
}

#[test]
fn a_method_call_prints_as_a_method_call() {
    // `sim` accessors are not implemented in the emitter yet, but the formatter
    // still has to write back a file that contains one rather than mangling it.
    let src = "lumen 1\neffect \"x\" {\n  let a = swarm.count(1, 2)\n  layer l { color = rgb(a, 0, 0) }\n}\n";
    let (file, diags) = parse(src);
    assert!(!diags.has_errors(), "{}", diags.render(src));
    let out = lumen_lang::fmt::format(&file.unwrap());
    assert!(out.contains("let a = swarm.count(1, 2)\n"), "{out}");
    let (again, _) = format_source(&out);
    assert_eq!(out, again.unwrap());
}

#[test]
fn a_number_too_small_to_print_comes_out_as_zero() {
    // Six decimal places is more than Q16.16 resolves, so anything under half a
    // millionth prints as `0` - never as `0.` with a dangling point, which
    // would not parse back.
    let src = "lumen 1\neffect \"x\" {\n  let tiny = 0.0000001\n  layer l { color = rgb(tiny, 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("let tiny = 0\n"), "{out}");
    assert!(!out.contains("0."), "a dangling decimal point:\n{out}");
}

#[test]
fn a_colour_keeps_its_alpha_only_when_it_has_one() {
    let src = "lumen 1\neffect \"x\" {\n  let a = #ff8000\n  let b = #ff800080\n  layer l { color = rgb(a, b, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("let a = #ff8000\n"), "{out}");
    assert!(out.contains("let b = #ff800080\n"), "{out}");
}

#[test]
fn a_string_keeps_its_escapes() {
    // The author, name and label fields all go through the same quoter. An
    // unescaped quote would produce a file that no longer parses.
    let src = "lumen 1\neffect \"a \\\"b\\\" c\" {\n  author \"back\\\\slash\"\n  param p : float = 0 range 0..1 label \"tab\\there\"\n  layer l { color = rgb(p, 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("effect \"a \\\"b\\\" c\" {"), "{out}");
    assert!(out.contains("author \"back\\\\slash\""), "{out}");
    assert!(out.contains("label \"tab\\there\""), "{out}");
}

#[test]
fn a_newline_inside_a_string_is_written_back_as_an_escape() {
    // A literal newline in the output would end the declaration halfway through
    // and the file would no longer parse - the one failure mode a formatter must
    // never have.
    let src = "lumen 1\neffect \"x\" {\n  author \"two\\nlines\"\n  layer l { color = rgb(1, 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("author \"two\\nlines\""), "{out}");
    assert!(
        !out.contains("author \"two\n"),
        "a raw newline escaped:\n{out}"
    );
}

#[test]
fn a_string_in_an_expression_position_is_printed_as_a_string() {
    // The emitter refuses a string as a value, but the formatter still has to
    // write back the file it was handed. Dropping the quotes would turn it into
    // an identifier and change the error the author sees next.
    let src = "lumen 1\neffect \"x\" {\n  let a = \"hi\"\n  layer l { color = rgb(a, 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("let a = \"hi\"\n"), "{out}");
}

// ---- Sim blocks ------------------------------------------------------------

#[test]
fn a_sim_if_without_an_else_closes_with_a_bare_brace() {
    // The `else`-less arm is a different code path from the one with an else,
    // and getting it wrong writes out a block that no longer parses.
    let src = r#"
lumen 1
effect "x" {
  sim swarm(count = 8) {
    let step = 1
    if step > 0 {
      step = 0
    }
    foreach p in particles {
      p.vel = p.vel * 0.5
    }
  }
  layer l { color = rgb(1, 0, 0) }
}
"#;
    let out = fmt_stable(src);
    assert!(out.contains("  sim swarm(count = 8) {\n"), "{out}");
    assert!(
        out.contains("    if step > 0 {\n      step = 0\n    }\n"),
        "{out}"
    );
    assert!(!out.contains("else"), "there was no else:\n{out}");
    assert!(
        out.contains("    foreach p in particles {\n      p.vel = p.vel * 0.5\n    }\n"),
        "{out}"
    );
}

// ---- Comments --------------------------------------------------------------

#[test]
fn a_trailing_comment_with_no_text_survives_as_a_bare_hash() {
    // An empty trailing comment is still a comment: dropping it changes the
    // file, and the "nothing is lost" contract has no exception for short ones.
    let src = "lumen 1\n\neffect \"x\" {\n  let a = 1 #\n  layer l { color = rgb(a, 0, 0) }\n}\n";
    let out = fmt_stable(src);
    assert!(out.contains("let a = 1 #\n"), "{out}");
    assert_eq!(comment_texts(src), comment_texts(&out));
}
