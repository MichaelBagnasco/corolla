mod app;
mod cache;
mod chart;
mod cleanup;
mod favorites;
mod patch;
mod preview;
mod scan;
mod snapshots;
mod theme;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::atomic::AtomicBool,
};

fn scan_summary(path: &Path) -> std::io::Result<()> {
    let scan = scan::scan(path, &AtomicBool::new(false), |_, _| {})?;
    let root = &scan.nodes[0];
    let items: Vec<_> = root
        .children
        .iter()
        .map(|&id| {
            let node = &scan.nodes[id];
            serde_json::json!({
                "name": node.name.to_string_lossy(),
                "bytes": node.bytes,
                "directory": node.is_dir
            })
        })
        .collect();
    let summary = serde_json::json!({
        "path": scan.root.to_string_lossy(),
        "bytes": root.bytes,
        "entries": scan.nodes.len() - 1,
        "skipped": scan.skipped,
        "items": items
    });
    serde_json::to_writer_pretty(std::io::stdout().lock(), &summary)?;
    println!();
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let initial = args.next();
    if initial.as_deref().is_some_and(|arg| arg == "scan") {
        let rest: Vec<_> = args.by_ref().collect();
        if let [flag, path] = rest.as_slice()
            && flag == "--json"
        {
            return match scan_summary(Path::new(path)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("corolla: {error}");
                    ExitCode::FAILURE
                }
            };
        }
        if !rest.is_empty() {
            eprintln!("Usage: corolla scan --json DIRECTORY");
            return ExitCode::from(2);
        }
    }
    if initial
        .as_deref()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "Corolla — radial disk explorer\n\nUsage: corolla [DIRECTORY]\n       corolla scan --json DIRECTORY"
        );
        return ExitCode::SUCCESS;
    }
    if args.next().is_some() {
        eprintln!("Usage: corolla [DIRECTORY]");
        std::process::exit(2);
    }
    let icon = image::load_from_memory(include_bytes!("../assets/corolla.png"))
        .expect("bundled icon")
        .into_rgba8();
    let viewport = egui::ViewportBuilder::default()
        .with_app_id("corolla")
        .with_icon(egui::IconData {
            width: icon.width(),
            height: icon.height(),
            rgba: icon.into_raw(),
        })
        .with_title("Corolla")
        .with_inner_size([1000.0, 680.0])
        .with_min_inner_size([640.0, 440.0]);
    match eframe::run_native(
        "corolla",
        eframe::NativeOptions {
            viewport,
            vsync: true,
            multisampling: 4,
            ..Default::default()
        },
        Box::new(move |cc| {
            theme::install_fonts(&cc.egui_ctx);
            Ok(Box::new(app::App::new(cc, initial.map(PathBuf::from))))
        }),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("corolla: {error}");
            ExitCode::FAILURE
        }
    }
}
