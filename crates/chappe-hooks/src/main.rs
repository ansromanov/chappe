fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = chappe_hooks::Env::from_process();
    let code = if args.first().map(String::as_str) == Some("hook") {
        // The hooks of a tool: the event arrives on standard input.
        chappe_hooks::hook::run_hook(
            &args,
            &env,
            &mut std::io::stdin(),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )
    } else {
        chappe_hooks::run(&args, &env, &mut std::io::stdout(), &mut std::io::stderr())
    };
    std::process::ExitCode::from(code as u8)
}
