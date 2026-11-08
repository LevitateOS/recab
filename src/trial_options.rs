//! The kernel options a trial boot's entry carries until it is committed,
//! one policy for every writer: the installed initramfs's fail-reboot word
//! when the entry boots through one (a root that cannot mount reboots
//! instead of opening a shell), the init's crash option (a dying PID 1
//! reboots), and a non-zero `panic=` (a panicking kernel reboots). An entry
//! loses the first two when it becomes the default, so a committed slot
//! that fails gets a shell to repair it rather than a reboot loop.

use distro_profile::InitSystem;
use recinit::live::FAIL_REBOOT_OPTION;

use crate::entry::{Entry, PANIC_KEY, PANIC_OPTION};
use crate::init_system::crash_option;

/// The words a trial of an `init` system's entry adds.
fn words(init: InitSystem, through_initramfs: bool) -> Vec<&'static str> {
    through_initramfs.then_some(FAIL_REBOOT_OPTION).into_iter().chain(crash_option(init)).collect()
}

/// A word some trial adds, whatever its init.
fn is_trial_word(option: &str) -> bool {
    option == FAIL_REBOOT_OPTION || InitSystem::ALL.iter().any(|init| crash_option(*init) == Some(option))
}

/// Make `entry` a trial of an `init` system.
pub(crate) fn make_trial(entry: &mut Entry, init: InitSystem) {
    let mut options: Vec<String> = entry.options().into_iter().filter(|o| !is_trial_word(o)).collect();
    // A trial must reboot on a panic, whatever `panic=` it had.
    let panic = options.iter().position(|o| o.starts_with(PANIC_KEY));
    match panic {
        Some(at) if options[at][PANIC_KEY.len()..].parse::<i64>().unwrap_or(0) <= 0 => options[at] = PANIC_OPTION.to_string(),
        Some(_) => {}
        None => options.push(PANIC_OPTION.to_string()),
    }
    options.extend(words(init, entry.has_initrd()).iter().map(|o| o.to_string()));
    entry.set_options(options);
}

/// Make `entry` a default's: no trial words.
pub(crate) fn make_default(entry: &mut Entry) {
    let options = entry.options().into_iter().filter(|o| !is_trial_word(o)).collect();
    entry.set_options(options);
}

/// `entry` carries a trial's options.
pub(crate) fn is_trial(entry: &Entry) -> bool {
    entry.options().iter().any(|o| is_trial_word(o))
}
