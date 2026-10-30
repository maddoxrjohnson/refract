// Keeps a release build from opening a console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    refract_app_lib::run()
}
