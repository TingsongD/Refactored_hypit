//! scene-script — sandboxed authored programs (the `<program>` element).
//!
//! A program is JS that never leaves its sandbox: no `std`/`os` modules
//! (the runtime is built without them), capped memory, capped
//! instructions. It draws by appending ops to a [`DrawList`] through a
//! canvas-shaped `ctx` — the rasterizer replays the list. Programs can
//! only ever produce *data*.

mod ops;
mod sandbox;

pub use ops::{DrawList, DrawOp};
pub use sandbox::{NullPrograms, Program, ProgramSource, SandboxPrograms, ScriptError};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_emits_ops_per_frame() {
        let mut p = Program::load(
            "function render(ctx, f, d) { ctx.rect(f, 1, 10, 10); ctx.text('n'+f, 0, 30); }",
        )
        .unwrap();
        let ops = p.render(7, "{}", 100.0, 50.0).unwrap();
        assert_eq!(
            ops.0,
            vec![
                DrawOp::Rect {
                    x: 7.0,
                    y: 1.0,
                    w: 10.0,
                    h: 10.0,
                    c: "#ffffffff".into()
                },
                DrawOp::Text {
                    t: "n7".into(),
                    x: 0.0,
                    y: 30.0,
                    size: 32.0,
                    c: "#ffffffff".into()
                },
            ]
        );
    }

    #[test]
    fn setup_runs_once_and_data_reaches_render() {
        let mut p = Program::load(
            r#"var n = 0;
               function setup(d) { n = d.base; }
               function render(ctx, f, d) { ctx.circle(d.base + n, 0, 5); }"#,
        )
        .unwrap();
        let data = r#"{"base": 40}"#;
        let a = p.render(0, data, 10.0, 10.0).unwrap();
        let b = p.render(1, data, 10.0, 10.0).unwrap();
        // setup ran once; render saw base+n = 80 both times
        assert_eq!(a.0[0], b.0[0]);
        match &a.0[0] {
            DrawOp::Circle { x, .. } => assert_eq!(*x, 80.0),
            _ => panic!("expected circle"),
        }
    }

    #[test]
    fn render_is_deterministic() {
        let mut p = Program::load(
            "function render(ctx, f, d) { for (let i=0;i<3;i++) ctx.rect(i*f,i,2,2); }",
        )
        .unwrap();
        let a = p.render(11, "{}", 0.0, 0.0).unwrap();
        let b = p.render(11, "{}", 0.0, 0.0).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn sandbox_has_no_filesystem_or_process() {
        // `std`/`os`/`require` don't exist — the runtime was built
        // without them, so there's nothing to reach for.
        let mut p = Program::load(
            "function render(ctx, f, d) { if (typeof std !== 'undefined' || typeof os !== 'undefined' || typeof require !== 'undefined') throw new Error('escape hatch'); ctx.rect(0,0,1,1); }",
        )
        .unwrap();
        p.render(0, "{}", 1.0, 1.0).unwrap();
    }

    #[test]
    fn bad_program_is_an_error_not_a_panic() {
        assert!(Program::load("this is not js ((").is_err());
        let mut p = Program::load("function render(ctx,f,d){ nope(); }").unwrap();
        assert!(p.render(0, "{}", 0.0, 0.0).is_err());
        // A program with no render fn errors on call, not on load.
        let mut p2 = Program::load("var x = 1;").unwrap();
        assert!(p2.render(0, "{}", 0.0, 0.0).is_err());
    }

    #[test]
    fn runaway_loop_is_aborted() {
        let mut p =
            Program::load("function render(ctx, f, d) { while (true) {} ctx.rect(0,0,1,1); }")
                .unwrap();
        assert!(p.render(0, "{}", 0.0, 0.0).is_err());
    }

    #[test]
    fn memory_hog_is_aborted() {
        let mut p = Program::load(
            "function render(ctx, f, d) { let a = []; while (true) a.push(new Array(1e6).fill(1)); }",
        )
        .unwrap();
        assert!(p.render(0, "{}", 0.0, 0.0).is_err());
    }

    #[test]
    fn malformed_ops_drop_but_good_ops_survive() {
        let (list, dropped) = DrawList::from_json(
            r##"[{"op":"rect","x":1,"y":2,"w":3,"h":4,"c":"#fff"},
                {"op":"bogus"}, 42]"##,
        );
        assert_eq!(list.0.len(), 1);
        assert_eq!(dropped, 2);
    }
}
