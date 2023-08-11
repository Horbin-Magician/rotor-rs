use module::searcher::Searcher;
mod module;
use system_tray::SystemTray;
mod system_tray;
use setting::Setting;
mod setting;

fn main() {
    let searcher = Searcher::new();
    let _setting = Setting::new();

    let _system_tray: SystemTray = SystemTray::new();

    searcher.show();
    // setting.show();
    slint::run_event_loop().unwrap();
}