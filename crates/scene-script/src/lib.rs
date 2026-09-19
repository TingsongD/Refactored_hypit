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
        let (ops, dropped) = p.render(7, "{}", 100.0, 50.0).unwrap();
        assert_eq!(dropped, 0);
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
        let a = p.render(0, data, 10.0, 10.0).unwrap().0;
        let b = p.render(1, data, 10.0, 10.0).unwrap().0;
        // setup ran once; render saw base+n = 80 both times
        assert_eq!(a.0[0], b.0[0]);
        match &a.0[0] {
            DrawOp::Circle { x, .. } => assert_eq!(*x, 80.0),
            _ => panic!("expected circle"),
        }
    }

    #[test]
    fn setup_state_on_d_reaches_render() {
        // The documented pattern: stash on `d` in setup, read it in
        // render — `d` is the same object every frame.
        let mut p = Program::load(
            r#"function setup(d) { d.total = (d.n || 0) * 2; }
               function render(ctx, f, d) { ctx.rect(d.total, 0, 5, 5); }"#,
        )
        .unwrap();
        let data = r#"{"n":21}"#; // the `with` object becomes `d` directly
        let (ops, _) = p.render(0, data, 10.0, 10.0).unwrap();
        match &ops.0[0] {
            DrawOp::Rect { x, .. } => assert_eq!(*x, 42.0),
            _ => panic!("expected rect"),
        }
        let (ops2, _) = p.render(5, data, 10.0, 10.0).unwrap();
        assert_eq!(ops.0[0], ops2.0[0]);
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

    #[test]
    fn program_sources_stay_inside_the_root() {
        let root = std::env::temp_dir().join(format!("prog-root-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("prog-out-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let good_src = "function render(ctx,f,d){ ctx.rect(0,0,1,1); }";
        std::fs::write(root.join("ok.js"), good_src).unwrap();
        std::fs::write(outside.join("escape.js"), good_src).unwrap();
        // A symlink inside the root pointing outside is containment too.
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("escape.js"), root.join("link.js")).unwrap();

        let mut progs = SandboxPrograms::new(root.clone());
        assert!(progs.ops("ok.js", 0, "{}", 10.0, 10.0).is_some());
        let rel = format!("../prog-out-{}", std::process::id());
        assert!(
            progs
                .ops(&format!("{rel}/escape.js"), 0, "{}", 10.0, 10.0)
                .is_none()
        );
        #[cfg(unix)]
        assert!(progs.ops("link.js", 0, "{}", 10.0, 10.0).is_none());
        // Absolute paths outside the root don't resolve either.
        assert!(
            progs
                .ops(
                    outside.join("escape.js").to_str().unwrap(),
                    0,
                    "{}",
                    10.0,
                    10.0
                )
                .is_none()
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn same_src_different_with_gets_independent_state() {
        // Two elements share one source file but pass different `with`
        // payloads — each must get its own runtime state, not the
        // first-seen `d`.
        let root = std::env::temp_dir().join(format!("prog-key-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("p.js"),
            "function setup(d){ d.x = (d.x||0) + d.step; }\n\
             function render(ctx,f,d){ d.x += d.step; ctx.rect(d.x,0,1,1); }",
        )
        .unwrap();
        let mut progs = SandboxPrograms::new(root.clone());
        let x = |list: DrawList| match &list.0[0] {
            DrawOp::Rect { x, .. } => *x,
            _ => panic!("rect"),
        };
        // step=1: setup adds 1, render adds 1 → 2.
        let a = progs.ops("p.js", 0, r#"{"step":1}"#, 1.0, 1.0).unwrap();
        assert_eq!(x(a), 2.0);
        // step=10 with a different payload must not inherit `d.x` = 2.
        let b = progs.ops("p.js", 0, r#"{"step":10}"#, 1.0, 1.0).unwrap();
        assert_eq!(x(b), 20.0);
        // And back on step=1 state continues where it left off (3, 4…).
        let c = progs.ops("p.js", 1, r#"{"step":1}"#, 1.0, 1.0).unwrap();
        assert_eq!(x(c), 3.0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
