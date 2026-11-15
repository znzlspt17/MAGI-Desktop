use std::{env, fs::File, process::ExitCode};

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("inspect") => {
            let path = args.next().ok_or("usage: inference inspect <model.gguf>")?;
            if args.next().is_some() { return Err("unexpected argument".into()); }
            let mut file = File::open(path).map_err(|e| e.to_string())?;
            let info = inference::inspect(&mut file)?;
            println!("{}", serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?);
            Ok(())
        }
        Some("diagnostic") => {
            let path = args.next().ok_or("usage: inference diagnostic <absolute-hip-dll-path>")?;
            if args.next().is_some() { return Err("unexpected argument".into()); }
            let info = inference::hip::diagnose(std::path::Path::new(&path))?;
            println!("{}", serde_json::to_string_pretty(&info).map_err(|e| e.to_string())?);
            Ok(())
        }
        Some("capability") if args.next().is_none() => {
            println!("{}", serde_json::json!({"inference_supported": false, "reason": "Gemma execution and the custom ternary storage ABI are not implemented", "gpu_runtime": "not probed"}));
            Ok(())
        }
        _ => Err("usage: inference inspect <model.gguf> | diagnostic <absolute-hip-dll-path> | capability".into()),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
