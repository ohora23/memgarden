//! Prints the settings this A/B turns on, resolved by the same loader the
//! daemon uses. Exists because the arms are worthless if the live value is
//! assumed rather than read.
fn main() {
    let cfg = memgarden_core::config::Config::load().expect("load config");
    println!("profile.name            = {:?}", cfg.profile.name);
    println!("retain.include_tool_calls = {}", cfg.retain.include_tool_calls);
    println!("retain.chunk_size       = {}", cfg.retain.chunk_size);
    println!("retain.max_initial_messages = {}", cfg.retain.max_initial_messages);
    println!("profile.retain_mission  = {:?}", cfg.profile.retain_mission);
    println!("hooks.mode              = {:?}", cfg.hooks.mode);
}
