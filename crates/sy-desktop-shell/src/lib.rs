//! Optional desktop applets and layer-shell stack bar.

pub mod bat;
pub mod bright;
pub mod bt;
pub mod cal;
pub mod disk;
pub mod gpu;
pub mod net;
pub mod notif;
pub mod npu;
pub mod popup;
pub mod profile;
pub mod silent;
pub mod sound;
pub mod stack;
pub mod vol;
pub mod wallpaper;
pub mod waybar;
pub mod wifi;
pub mod wwan;

#[cfg(feature = "mon-exporter")]
mod mon_exporter;

/// Whether an executable can be found on the current search path.
pub fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}
