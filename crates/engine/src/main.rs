mod commands;
mod report;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "engine",
    version,
    about = "Scene engine — agent-authored video, timed by words",
    long_about = "Compile .scene documents into rendered video. Anchored to what is said, not to when it happens to land."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scaffold a scene project directory.
    Init { dir: PathBuf },
    /// Parse and validate a .scene file; report every problem with spans.
    Check { file: PathBuf },
    /// Print the lowered scene IR as JSON.
    Parse { file: PathBuf },
    /// Build a --timings JSON from a markers file or WhisperX output.
    Align {
        file: PathBuf,
        /// Markers file: `cue start_s end_s` per line.
        #[arg(long)]
        markers: Option<PathBuf>,
        /// WhisperX service JSON output file.
        #[arg(long)]
        whisperx: Option<PathBuf>,
        /// Output path — defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Render a scene to mp4.
    Render {
        file: PathBuf,
        /// JSON TimingMap file (alignment connector output shape).
        #[arg(long)]
        timings: Option<PathBuf>,
        /// Output path — defaults to the scene's <render target>.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Frame range `start:end` — defaults to the whole program.
        #[arg(long)]
        frames: Option<String>,
        /// Render worker threads.
        #[arg(long, default_value_t = 4)]
        workers: usize,
    },
    /// Probe external tools the engine shells out to.
    Doctor,
    /// Ingest media (file or URL), shot-detect, and emit a draft .scene.
    Adapt {
        /// Media file path, or an http(s) URL fetched via yt-dlp.
        source: String,
        /// Directory for fetched media + the emitted .scene.
        #[arg(long, default_value = "assets")]
        out_dir: PathBuf,
        /// Where the draft .scene lands — defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Localhost test UI: edit markup, check, render, watch the result.
    Ui {
        /// Project dir containing assets/ (+ main.scene, timings.json).
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Port to bind on localhost.
        #[arg(long, default_value_t = 8484)]
        port: u16,
        /// Don't auto-open the browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Inspect or invoke capabilities declared in a scene.toml.
    Cap {
        /// Path to the project's scene.toml.
        #[arg(long, default_value = "scene.toml")]
        config: PathBuf,
        #[command(subcommand)]
        action: CapCmd,
    },
}

#[derive(Subcommand)]
enum CapCmd {
    /// List registered capabilities.
    List,
    /// Invoke a connector: request JSON in, asset file out.
    Call {
        /// Capability name from the registry.
        name: String,
        /// Params object, as inline JSON.
        #[arg(long, default_value = "{}")]
        params: String,
        /// Where the asset lands.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match &cli.command {
        Cmd::Init { dir } => commands::init(dir),
        Cmd::Check { file } => commands::check(file),
        Cmd::Parse { file } => commands::parse(file),
        Cmd::Align {
            file,
            markers,
            whisperx,
            out,
        } => commands::align(
            file,
            markers.as_deref(),
            whisperx.as_deref(),
            out.as_deref(),
        ),
        Cmd::Render {
            file,
            timings,
            out,
            frames,
            workers,
        } => commands::render(
            file,
            timings.as_deref(),
            out.as_deref(),
            frames.as_deref(),
            *workers,
        ),
        Cmd::Doctor => commands::doctor(),
        Cmd::Adapt {
            source,
            out_dir,
            out,
        } => commands::adapt(source, out_dir, out.as_deref()),
        Cmd::Ui { dir, port, no_open } => ui::serve(dir, *port, !no_open),
        Cmd::Cap { config, action } => match action {
            CapCmd::List => commands::cap_list(config),
            CapCmd::Call { name, params, out } => commands::cap_call(config, name, params, out),
        },
    };
    ExitCode::from(code as u8)
}
