//! The window: a disk sidebar, a toolbar of disk tasks, the selected disk's
//! summary and contents, and sheets that ask, show progress, and report.
use super::*;
use carryfs_core::ObjectKind;
use model::{bytes, kind, permissions};
use platform::DeviceKind;
use rune::tok::{self, TextStyle};
use rune::{Cx, El, FileDialogOptions, Icon, LiveRegion, ScrollSpec, col, el, icon, row, text};
use rune_kit::{Button, ButtonVariant, Dialog, Kit, PaletteSeed, Progress, ScrollBar, TextInput};

macro_rules! glyph {
    ($name:ident, $body:expr) => {
        static $name: Icon = Icon {
            name: concat!("carryfs-", stringify!($name)),
            view: 20.0,
            body: $body,
        };
    };
}
// Each body repeats the stroke attributes: icons don't inherit from groups.
glyph!(
    DRIVE,
    r#"<rect x="2.5" y="5.5" width="15" height="9" rx="2" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M5.5 10H10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><circle cx="14" cy="10" r="1" fill="currentColor"/>"#
);
glyph!(
    VOLUME,
    r#"<ellipse cx="10" cy="5.5" rx="6" ry="2.5" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M4 5.5V14.5C4 15.9 6.7 17 10 17S16 15.9 16 14.5V5.5M4 10C4 11.4 6.7 12.5 10 12.5S16 11.4 16 10" stroke="currentColor" stroke-width="1.5" fill="none"/>"#
);
glyph!(
    IMAGE,
    r#"<path d="M5 2.5H11.5L15.5 6.5V17.5H5Z M11.5 2.5V6.5H15.5" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/><circle cx="10.25" cy="12" r="2.5" stroke="currentColor" stroke-width="1.5" fill="none"/>"#
);
glyph!(
    FIRST_AID,
    r#"<path d="M8 3.5H12V8H16.5V12H12V16.5H8V12H3.5V8H8Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    ERASE,
    r#"<path d="M3.6 11.4L10.4 4.6A1.5 1.5 0 0 1 12.5 4.6L15.9 8A1.5 1.5 0 0 1 15.9 10.1L9.5 16.5H6.9L3.6 13.2A1.3 1.3 0 0 1 3.6 11.4Z M7.5 7.5L12.5 12.5 M9.5 16.5H16.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    RECOVER,
    r#"<circle cx="10" cy="10" r="7" stroke="currentColor" stroke-width="1.5" fill="none"/><circle cx="10" cy="10" r="3" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M5 5L7.9 7.9M12.1 12.1L15 15M15 5L12.1 7.9M7.9 12.1L5 15" stroke="currentColor" stroke-width="1.5" fill="none"/>"#
);
glyph!(
    MOUNT,
    r#"<path d="M10 13.5V3.5M6 7.5L10 3.5L14 7.5M4 16.5H16" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    UNMOUNT,
    r#"<path d="M10 3.5V13.5M6 9.5L10 13.5L14 9.5M4 16.5H16" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    EJECT,
    r#"<path d="M10 4L16 11H4Z M4 15.5H16" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    LOCKED,
    r#"<rect x="4.5" y="9" width="11" height="8" rx="1.5" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M7 9V6.5A3 3 0 0 1 13 6.5V9" stroke="currentColor" stroke-width="1.5" fill="none"/>"#
);
glyph!(
    UNLOCKED,
    r#"<rect x="4.5" y="9" width="11" height="8" rx="1.5" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M7 9V6.5A3 3 0 0 1 12.8 5.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/>"#
);
glyph!(
    SOFTWARE,
    r#"<circle cx="10" cy="10" r="7" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M10 6.5V13M7 10L10 13L13 10" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    PLUS,
    r#"<path d="M10 4.5V15.5M4.5 10H15.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/>"#
);
glyph!(
    REFRESH,
    r#"<path d="M15.5 10A5.5 5.5 0 1 1 13.9 6.1M15.5 3.5V6.5H12.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    OK,
    r#"<circle cx="10" cy="10" r="7.5" stroke="currentColor" stroke-width="1.5" fill="none"/><path d="M6.8 10.2L9 12.4L13.3 7.8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    WARNING,
    r#"<path d="M10 3L17.5 16.5H2.5Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/><path d="M10 8V11.5M10 14V14.1" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/>"#
);
glyph!(
    FOLDER,
    r#"<path d="M2.5 5.5A1 1 0 0 1 3.5 4.5H7.5L9 6H16.5A1 1 0 0 1 17.5 7V15A1 1 0 0 1 16.5 16H3.5A1 1 0 0 1 2.5 15Z" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    FILE,
    r#"<path d="M5 2.5H11.5L15.5 6.5V17.5H5Z M11.5 2.5V6.5H15.5" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round" fill="none"/>"#
);
glyph!(
    LINK,
    r#"<path d="M8.5 11.5L11.5 8.5M7 9.5L5.5 11A2.5 2.5 0 0 0 9 14.5L10.5 13M13 10.5L14.5 9A2.5 2.5 0 0 0 11 5.5L9.5 7" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" fill="none"/>"#
);

fn label(value: impl Into<String>, k: &Kit) -> El {
    text(value).font(k.ty.body).color(k.c.text_base)
}
fn muted(value: impl Into<String>, k: &Kit) -> El {
    text(value).font(k.ty.text_small).color(k.c.text_weak)
}
fn caption(value: impl Into<String>, k: &Kit) -> El {
    text(value.into().to_uppercase())
        .font(TextStyle {
            tracking: 0.4,
            ..k.ty.text_xsmall_medium
        })
        .color(k.c.text_faint)
}
fn rule(k: &Kit) -> El {
    el().w_full().h(1.0).fill(k.c.border_weak)
}
fn heading(title: impl Into<String>, k: &Kit) -> El {
    text(title)
        .font(TextStyle::new(22.0, 600.0))
        .color(k.c.text_base)
        .truncate()
}

pub fn view(s: &State, _: &mut Cx<State>) -> El {
    let t = tok::theme(false);
    let k = Kit::from_palette(PaletteSeed {
        bg: t.bg,
        fg: t.text,
        accent: tok::accent(0, false),
    });
    let k = &k;
    let content = if s.tab == Tab::Software {
        scrolled(software(s, k), k)
    } else if s.browser.is_none() && s.damaged.is_none() && s.device.is_none() {
        empty(k)
    } else {
        let body = match s.tab {
            Tab::Files => files(s, k),
            Tab::Activity => scrolled(activity(s, k), k),
            _ => scrolled(overview(s, k), k),
        };
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .padding(24.0)
            .gap(20.0)
            .child(summary(s, k))
            .child(tabs(s, k))
            .child(body)
    };
    let main = col()
        .grow(1.0)
        .min_w(0.0)
        .h_full()
        .fill(k.c.bg_base)
        .child(toolbar(s, k))
        .child(rule(k))
        .child(content)
        .child(status_bar(s, k));
    let mut layer = Vec::new();
    sheet(s, k, &mut layer);
    rune_kit::root(
        row().w_full().h_full().child(sidebar(s, k)).child(main),
        layer,
    )
}

fn scrolled(body: El, k: &Kit) -> El {
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(body)
        .scroll_bar(k)
}

// ---------------------------------------------------------------- sidebar

pub(crate) fn device_rows(s: &State) -> Vec<(usize, usize)> {
    if !s.show_system {
        return s
            .devices
            .iter()
            .enumerate()
            .filter(|(_, d)| d.visible_by_default(&s.devices))
            .map(|(i, _)| (i, 0))
            .collect();
    }
    fn append(s: &State, index: usize, depth: usize, rows: &mut Vec<(usize, usize)>) {
        if rows.iter().any(|(i, _)| *i == index) {
            return;
        }
        rows.push((index, depth));
        let device = &s.devices[index];
        if s.expanded_devices.contains(&device.id) {
            for (child, _) in s
                .devices
                .iter()
                .enumerate()
                .filter(|(_, d)| d.parent.as_deref() == Some(&device.id))
            {
                append(s, child, depth + 1, rows);
            }
        }
    }
    let mut rows = Vec::new();
    for (i, d) in s.devices.iter().enumerate() {
        if !s.devices.iter().any(|p| Some(&p.id) == d.parent.as_ref()) {
            append(s, i, 0, &mut rows);
        }
    }
    rows
}

/// A sidebar row: icon, name, and a second line; `chosen` is the selection.
fn source_row(
    k: &Kit,
    glyph: &'static Icon,
    title: String,
    detail: String,
    chosen: bool,
    busy: bool,
) -> El {
    let ink = if chosen {
        k.c.icon_accent
    } else {
        k.c.icon_muted
    };
    row()
        .w_full()
        .min_w(0.0)
        .h(40.0)
        .px(8.0)
        .gap(8.0)
        .items_center()
        .radius(6.0)
        .fill(if chosen {
            k.c.surface_hover
        } else {
            k.c.layer_01
        })
        .hover(k.c.hover)
        .focusable(!busy)
        .disabled(busy)
        .focus_selected(chosen)
        .label(title.clone())
        .child(icon(glyph, 18.0).color(ink))
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(1.0)
                .child(
                    text(title)
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base)
                        .truncate(),
                )
                .child(
                    text(detail)
                        .font(k.ty.text_xsmall)
                        .color(k.c.text_weak)
                        .truncate(),
                ),
        )
}

fn sidebar(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let mut disks = col().w_full().gap(2.0).role("tree").focus_group();
    let visible = device_rows(s);
    if visible.is_empty() {
        disks = disks.child(
            muted(
                if busy {
                    "Looking for disks…"
                } else {
                    "Connect a drive to see it here."
                },
                k,
            )
            .wrap()
            .px(8.0),
        );
    }
    for (index, depth) in visible {
        let device = &s.devices[index];
        let chosen = s.device.as_ref().is_some_and(|d| d.id == device.id);
        let has_children = s.show_system
            && s.devices
                .iter()
                .any(|d| d.parent.as_deref() == Some(&device.id));
        let expanded = s.expanded_devices.contains(&device.id);
        let mut line = row()
            .w_full()
            .min_w(0.0)
            .items_center()
            .pl(depth as f32 * 14.0);
        if has_children {
            let id = device.id.clone();
            line = line.child(
                rune_kit::IconButton::new(if expanded {
                    k.glyph.triangle_down
                } else {
                    k.glyph.triangle_right
                })
                .variant(rune_kit::IconButtonVariant::Ghost)
                .label(format!(
                    "{} {}",
                    if expanded { "Collapse" } else { "Expand" },
                    device.name
                ))
                .expanded(expanded)
                .on_press(move |s: &mut State| {
                    if !s.expanded_devices.remove(&id) {
                        s.expanded_devices.insert(id.clone());
                    }
                })
                .build(k)
                .key(format!("expand-{}", device.id)),
            );
        }
        let detail = if s.show_system {
            format!(
                "{} · {}",
                device.id,
                match device.kind {
                    DeviceKind::Physical => "Physical",
                    DeviceKind::Virtual => "Virtual",
                    DeviceKind::Unknown => "Kind not reported",
                }
            )
        } else {
            format!(
                "{} · {}{}",
                bytes(device.bytes),
                device.format_name(),
                if device.mount.is_some() {
                    " · Mounted"
                } else {
                    ""
                }
            )
        };
        let selected = device.clone();
        let mut entry = source_row(
            k,
            if device.whole { &DRIVE } else { &VOLUME },
            device.name.clone(),
            detail,
            chosen,
            busy,
        )
        .grow(1.0)
        .role("treeitem")
        .on_press_tx(move |s: &mut State, tx| {
            if s.pending.is_none() {
                s.select_device(selected.clone(), tx);
            }
        })
        .key(format!("device-{index}"));
        if has_children {
            entry = entry.expanded(expanded);
        }
        disks = disks.child(line.child(entry));
    }
    let mut images = col().w_full().gap(2.0).role("listbox").focus_group();
    for (index, path) in s.images.iter().enumerate() {
        let chosen = s
            .source()
            .is_some_and(|source| source.device.is_none() && source.path == *path);
        let file = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let title = s
            .browser
            .as_ref()
            .filter(|b| b.device.is_none() && b.path == *path)
            .map_or(file.clone(), |b| b.info.name());
        let image = path.clone();
        images = images.child(
            source_row(k, &IMAGE, title, file, chosen, busy)
                .role("option")
                .on_press_tx(move |s: &mut State, tx| s.open(image.clone(), tx))
                .key(format!("image-{index}")),
        );
    }
    let update = s.versions.update_available();
    col()
        .w(250.0)
        .h_full()
        .px(12.0)
        .pt(16.0)
        .pb(12.0)
        .gap(8.0)
        .fill(k.c.layer_01)
        .child(
            row()
                .items_center()
                .px(8.0)
                .child(caption("Disks", k).grow(1.0))
                .child(
                    rune_kit::IconButton::new(&REFRESH)
                        .variant(rune_kit::IconButtonVariant::Ghost)
                        .label("Refresh disks")
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.discover(tx))
                        .build(k)
                        .key("refresh-devices"),
                ),
        )
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .key("device-list")
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(disks)
                .scroll_bar(k),
        )
        .child(
            row()
                .items_center()
                .px(8.0)
                .pt(8.0)
                .child(caption("Disk images", k).grow(1.0))
                .child(
                    rune_kit::IconButton::new(&PLUS)
                        .variant(rune_kit::IconButtonVariant::Ghost)
                        .label("Open disk image")
                        .disabled(busy)
                        .on_press_tx(open_dialog)
                        .build(k)
                        .key("open-image"),
                ),
        )
        .child(
            col()
                .w_full()
                .max_h(132.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(if s.images.is_empty() {
                    muted("Open a .cfs disk image with +.", k).px(8.0)
                } else {
                    images
                })
                .scroll_bar(k),
        )
        .child(rule(k))
        .child(
            Button::new(if s.show_system {
                "Show only external disks"
            } else {
                "Show all disks"
            })
            .variant(ButtonVariant::GhostMuted)
            .disabled(busy)
            .on_press(|s: &mut State| s.show_system = !s.show_system)
            .build(k)
            .key("show-system"),
        )
        .child(
            source_row(
                k,
                &SOFTWARE,
                "Software Update".into(),
                if update {
                    format!(
                        "CarryFS {} available",
                        s.versions.latest.as_deref().unwrap_or("")
                    )
                } else {
                    format!(
                        "Installed: {}",
                        s.versions.installed.as_deref().unwrap_or("not installed")
                    )
                },
                s.tab == Tab::Software,
                busy,
            )
            .role("button")
            .description(if s.tab == Tab::Software {
                "Current view"
            } else {
                "Show software versions and updates"
            })
            .on_press(|s: &mut State| s.tab = Tab::Software)
            .key("software"),
        )
}

pub(crate) fn open_dialog(_: &mut State, tx: &mut Tx) {
    tx.open_file(
        FileDialogOptions {
            title: "Open CarryFS disk image".into(),
            ..Default::default()
        },
        |s: &mut State, result, tx| match result {
            Ok(Some(path)) => s.open(path, tx),
            Ok(None) => {}
            Err(e) => s.apply(Err(e.to_string())),
        },
    );
}

// ---------------------------------------------------------------- toolbar

/// A Disk Utility style toolbar button: icon over label.
fn tool(
    k: &Kit,
    glyph: &'static Icon,
    title: &str,
    key: &str,
    enabled: bool,
    help: &str,
    press: impl Fn(&mut State, &mut Tx) + 'static,
) -> El {
    let mut button = col()
        .key(key)
        .w(68.0)
        .py(6.0)
        .gap(3.0)
        .items_center()
        .radius(6.0)
        .role("button")
        .label(title)
        .help(help)
        .focusable(enabled)
        .disabled(!enabled)
        .child(icon(glyph, 20.0).color(if enabled {
            k.c.icon_base
        } else {
            k.c.icon_faint
        }))
        .child(text(title).font(k.ty.text_xsmall).color(if enabled {
            k.c.text_base
        } else {
            k.c.text_faint
        }));
    if enabled {
        button = button.hover(k.c.hover).on_press_tx(press);
    }
    button
}

fn toolbar(s: &State, k: &Kit) -> El {
    let idle = s.pending.is_none();
    let mut bar = row()
        .w_full()
        .h(64.0)
        .px(16.0)
        .gap(2.0)
        .items_center()
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(2.0)
                .child(
                    text("CarryFS Manager")
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base),
                )
                .child(muted(
                    if s.tab == Tab::Software {
                        "Software Update".to_owned()
                    } else {
                        s.source()
                            .map(|source| source.name())
                            .or_else(|| s.device.as_ref().map(|d| d.name.clone()))
                            .unwrap_or_else(|| "No disk selected".into())
                    },
                    k,
                )),
        );
    if s.tab != Tab::Software {
        let source = s.source().is_some();
        let device = s.device.as_ref();
        let mounted = device.is_some_and(|d| d.mount.is_some());
        let erasable = device.is_some_and(|d| d.external_partition() && d.mount.is_none());
        let changeable = device.is_some_and(|d| !d.protected() && d.carryfs() && !d.whole);
        let ejectable = device.is_some_and(|d| !d.protected() && d.uuid.is_some());
        bar = bar
            .child(tool(
                k,
                &FIRST_AID,
                "First Aid",
                "first-aid",
                idle && source,
                if mounted {
                    "Unmount the volume to run First Aid."
                } else {
                    "Check the volume's structure and file data."
                },
                |s, _| s.ask(SheetKind::FirstAid),
            ))
            .child(tool(
                k,
                &ERASE,
                "Erase",
                "erase",
                idle && erasable,
                if mounted {
                    "Unmount the partition to erase it."
                } else {
                    "Erase an external partition as CarryFS."
                },
                |s, _| s.ask(SheetKind::Erase),
            ))
            .child(tool(
                k,
                &RECOVER,
                "Recover",
                "recover-tool",
                idle && source,
                "Copy files to a new folder or disk image.",
                |s, _| s.ask(SheetKind::Recover),
            ))
            .child(tool(
                k,
                if mounted { &UNMOUNT } else { &MOUNT },
                if mounted { "Unmount" } else { "Mount" },
                "mount",
                idle && changeable,
                "Mount or unmount the CarryFS volume.",
                |s, tx| {
                    let action = if s.device.as_ref().is_some_and(|d| d.mount.is_some()) {
                        DeviceAction::Unmount
                    } else {
                        DeviceAction::Mount
                    };
                    s.device_action(action, tx);
                },
            ))
            .child(tool(
                k,
                &EJECT,
                "Eject",
                "eject",
                idle && ejectable,
                "Eject the disk and all its volumes.",
                |s, _| s.ask(SheetKind::Eject),
            ));
    }
    let open = unlocked();
    bar.child(el().w(1.0).h(32.0).mx(8.0).fill(k.c.border_weak))
        .child(tool(
            k,
            if open { &UNLOCKED } else { &LOCKED },
            if open { "Lock" } else { "Unlock" },
            "lock",
            idle,
            if open {
                "Disk operations are allowed until you quit. Lock to ask for your password again."
            } else {
                "Allow disk operations with your administrator password, once until you quit."
            },
            |s, tx| s.toggle_lock(tx),
        ))
}

// ---------------------------------------------------------------- summary

fn summary(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let (glyph, name, subtitle, capacity): (&'static Icon, String, String, Option<u64>) =
        if let Some(b) = &s.browser {
            (
                if b.device.is_some() { &VOLUME } else { &IMAGE },
                b.info.name(),
                format!(
                    "CarryFS volume · Format {} · {} · Read-only",
                    b.info.revision,
                    if b.device.is_some() {
                        "External"
                    } else {
                        "Disk image"
                    }
                ),
                Some(b.info.byte_len),
            )
        } else if let Some(d) = &s.damaged {
            (
                if d.source.device.is_some() {
                    &VOLUME
                } else {
                    &IMAGE
                },
                d.source.name(),
                "CarryFS volume · Can't be opened".into(),
                d.source.device.as_ref().map(|d| d.bytes),
            )
        } else if let Some(d) = &s.device {
            (
                if d.whole { &DRIVE } else { &VOLUME },
                d.name.clone(),
                format!(
                    "{} · {} · {}",
                    if d.whole { "Disk" } else { d.format_name() },
                    if d.internal { "Internal" } else { "External" },
                    match &d.mount {
                        Some(mount) => format!("Mounted at {}", mount.display()),
                        None if d.whole => d.id.clone(),
                        None => "Not mounted".into(),
                    }
                ),
                Some(d.bytes),
            )
        } else {
            return el();
        };
    let mut header = row()
        .w_full()
        .gap(16.0)
        .items_center()
        .child(
            el().size(56.0, 56.0)
                .radius(12.0)
                .fill(k.c.layer_01)
                .center()
                .child(icon(glyph, 32.0).color(k.c.icon_base)),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.0)
                .gap(4.0)
                .child(heading(name, k))
                .child(muted(subtitle, k).truncate()),
        );
    if let Some(capacity) = capacity {
        header = header.child(
            col()
                .items_end()
                .gap(2.0)
                .child(
                    text(bytes(capacity))
                        .font(TextStyle::new(20.0, 500.0))
                        .color(k.c.text_base),
                )
                .child(muted("Capacity", k)),
        );
    }
    if s.source().is_some_and(|source| source.device.is_none()) {
        header = header.child(
            Button::new("Close")
                .variant(ButtonVariant::Outline)
                .disabled(busy)
                .on_press(|s: &mut State| {
                    s.browser = None;
                    s.damaged = None;
                    s.selected = None;
                    s.status = "Disk image closed.".into();
                    s.error = false;
                })
                .build(k)
                .key("close-image"),
        );
    }
    let mut card = col()
        .w_full()
        .gap(16.0)
        .padding(20.0)
        .radius(10.0)
        .fill(k.c.surface_raised_base)
        .border(1.0, k.c.border_weak)
        .child(header);
    if let Some(b) = &s.browser {
        let block = u64::from(b.space.block_bytes);
        let total = b.info.byte_len;
        let free = b.info.free_blocks * block;
        let available = b.space.available_blocks * block;
        let used = total.saturating_sub(free);
        let reserved = free.saturating_sub(available);
        let used_color = tok::accent(0, false);
        let reserved_color = k.c.border_strong;
        let legend = |color, title: &str, value: u64| {
            row()
                .gap(8.0)
                .items_center()
                .child(el().size(10.0, 10.0).radius(3.0).fill(color))
                .child(
                    col()
                        .child(
                            text(title.to_owned())
                                .font(k.ty.text_small_medium)
                                .color(k.c.text_base),
                        )
                        .child(muted(bytes(value), k)),
                )
        };
        card = card
            .child(
                row()
                    .w_full()
                    .h(14.0)
                    .radius(4.0)
                    .clip_soft(0.0)
                    .fill(k.c.surface_base)
                    .border(1.0, k.c.border_weak)
                    .child(el().h_full().grow(used as f32).fill(used_color))
                    .child(el().h_full().grow(reserved as f32).fill(reserved_color))
                    .child(el().h_full().grow(available as f32)),
            )
            .child(
                row()
                    .gap(32.0)
                    .child(legend(used_color, "Used", used))
                    .child(legend(reserved_color, "Reserved", reserved))
                    .child(legend(k.c.surface_base, "Available", available)),
            );
    }
    card
}

fn tabs(s: &State, k: &Kit) -> El {
    let mut strip = row()
        .gap(2.0)
        .padding(2.0)
        .radius(8.0)
        .fill(k.c.layer_01)
        .role("tablist")
        .focus_group();
    for (tab, title, key) in [
        (Tab::Overview, "Overview", "tab-overview"),
        (Tab::Files, "Files", "tab-files"),
        (Tab::Activity, "History", "tab-activity"),
    ] {
        let chosen = s.tab == tab;
        strip = strip.child(
            el().key(key)
                .px(14.0)
                .py(5.0)
                .radius(6.0)
                .fill(if chosen {
                    k.c.surface_raised_base
                } else {
                    k.c.layer_01
                })
                .hover(k.c.hover)
                .role("tab")
                .label(title)
                .focusable(true)
                .focus_selected(chosen)
                .on_press(move |s: &mut State| s.tab = tab)
                .child(text(title).font(k.ty.ui_medium).color(if chosen {
                    k.c.text_base
                } else {
                    k.c.text_weak
                })),
        );
    }
    row().child(strip)
}

fn empty(k: &Kit) -> El {
    col()
        .w_full()
        .grow(1.0)
        .center()
        .gap(12.0)
        .child(icon(&DRIVE, 56.0).color(k.c.icon_faint))
        .child(heading("Select a disk", k))
        .child(muted(
            "Connect a drive formatted as CarryFS, or open a disk image with + in the sidebar.",
            k,
        ))
        .child(
            Button::new("Open Disk Image…")
                .variant(ButtonVariant::Outline)
                .on_press_tx(open_dialog)
                .build(k)
                .key("open-image-empty"),
        )
}

// ---------------------------------------------------------------- overview

/// A highlighted message with actions, for the state that blocks the usual tasks.
fn callout(k: &Kit, warning: bool, title: &str, message: String, actions: Vec<El>) -> El {
    let mut card = col()
        .w_full()
        .gap(12.0)
        .padding(16.0)
        .radius(10.0)
        .fill(k.c.surface_raised_base)
        .border(
            1.0,
            if warning {
                k.c.warning.icon
            } else {
                k.c.border_weak
            },
        )
        .child(
            row()
                .gap(10.0)
                .items_center()
                .child(
                    icon(if warning { &WARNING } else { &LOCKED }, 18.0).color(if warning {
                        k.c.warning.icon
                    } else {
                        k.c.icon_muted
                    }),
                )
                .child(
                    text(title.to_owned())
                        .font(k.ty.ui_medium)
                        .color(k.c.text_base),
                ),
        )
        .child(label(message, k).wrap_anywhere());
    if !actions.is_empty() {
        card = card.child(row().gap(8.0).children(actions));
    }
    card
}

/// One First Aid result: a status line and the tool's counts.
fn health_tile(k: &Kit, title: &str, result: Option<&ScanResult>, never: &str) -> El {
    let (glyph, color, state) = match result {
        None => (&FIRST_AID, k.c.icon_faint, never.to_owned()),
        Some(r) => {
            let seconds =
                r.at.duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
            (
                if r.passed { &OK } else { &WARNING },
                if r.passed {
                    k.c.success.icon
                } else {
                    k.c.warning.icon
                },
                format!(
                    "{} · {:02}:{:02} UTC",
                    if r.passed {
                        "No problems found"
                    } else {
                        "Problems found"
                    },
                    seconds / 3600 % 24,
                    seconds / 60 % 60
                ),
            )
        }
    };
    let counts = result.and_then(|r| r.summary.lines().next()).map(|line| {
        line.split_once(": ")
            .map_or(line, |(_, counts)| counts)
            .to_owned()
    });
    let mut body = col()
        .grow(1.0)
        .min_w(0.0)
        .gap(2.0)
        .child(
            text(title.to_owned())
                .font(k.ty.text_small_medium)
                .color(k.c.text_weak),
        )
        .child(label(state, k));
    if let Some(counts) = counts {
        body = body.child(muted(counts, k).wrap());
    }
    row()
        .grow(1.0)
        .min_w(0.0)
        .gap(12.0)
        .padding(14.0)
        .radius(10.0)
        .border(1.0, k.c.border_weak)
        .child(icon(glyph, 22.0).color(color))
        .child(body)
}

/// Disk Utility's two-column table of facts.
fn facts(k: &Kit, pairs: Vec<(&str, String)>) -> El {
    let half = pairs.len().div_ceil(2);
    let column = |pairs: &[(&str, String)]| {
        let mut list = col().grow(1.0).min_w(0.0);
        for (index, (name, value)) in pairs.iter().enumerate() {
            if index > 0 {
                list = list.child(rule(k));
            }
            list = list.child(
                row()
                    .w_full()
                    .py(8.0)
                    .gap(12.0)
                    .child(muted(*name, k).w(130.0))
                    .child(label(value.clone(), k).grow(1.0).min_w(0.0).wrap_anywhere()),
            );
        }
        list
    };
    row()
        .w_full()
        .gap(32.0)
        .padding(16.0)
        .radius(10.0)
        .border(1.0, k.c.border_weak)
        .child(column(&pairs[..half]))
        .child(column(&pairs[half..]))
}

fn overview(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let mut body = col().w_full().gap(20.0).pb(8.0);
    if let Some(d) = &s.damaged {
        body = body.child(callout(
            k,
            true,
            "This volume can't be opened",
            format!(
                "Its metadata is damaged ({}). Run First Aid to see what can be repaired, or recover its files to a new place.",
                d.error
            ),
            vec![
                Button::new("Run First Aid…")
                    .variant(ButtonVariant::Submit)
                    .disabled(busy)
                    .on_press(|s: &mut State| s.ask(SheetKind::FirstAid))
                    .build(k)
                    .key("diagnose"),
                Button::new("Recover Files…")
                    .disabled(busy)
                    .on_press(|s: &mut State| s.ask(SheetKind::Recover))
                    .build(k),
            ],
        ));
    } else if s.browser.is_none()
        && let Some(d) = &s.device
    {
        body = body.child(if d.protected() {
            callout(
                k,
                false,
                "System disk",
                "CarryFS Manager doesn't change internal or system disks.".into(),
                vec![],
            )
        } else if d.whole && !d.carryfs() {
            callout(
                k,
                false,
                "Select a volume",
                "This is a whole disk. Select one of its volumes to work with it.".into(),
                vec![],
            )
        } else if let Some(mount) = &d.mount {
            let path = mount.clone();
            callout(
                k,
                false,
                "Mounted",
                format!(
                    "{} is in use at {}. Unmount it to run First Aid, recover files, or erase it.",
                    d.name,
                    mount.display()
                ),
                vec![
                    Button::new("Show in Finder")
                        .on_press(move |s: &mut State| {
                            if let Err(error) = platform::show(&path) {
                                s.apply(Err(error.to_string()));
                            }
                        })
                        .build(k)
                        .key("show-in-finder"),
                ],
            )
        } else if d.carryfs() {
            callout(
                k,
                false,
                "Unlock to look inside",
                "Reading a disk directly needs your administrator password. CarryFS Manager asks once and keeps the permission until you quit.".into(),
                vec![
                    Button::new("Unlock and Open")
                        .variant(ButtonVariant::Submit)
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.toggle_lock(tx))
                        .build(k)
                        .key("inspect-device"),
                ],
            )
        } else {
            callout(
                k,
                false,
                &format!("Formatted as {}", d.format_name()),
                "To use this partition with CarryFS, erase it. Erasing deletes everything on it.".into(),
                vec![
                    Button::new("Erase…")
                        .disabled(busy || !d.external_partition())
                        .on_press(|s: &mut State| s.ask(SheetKind::Erase))
                        .build(k)
                        .key("erase-callout"),
                ],
            )
        });
    }
    if let Some(source) = s.source() {
        let (plan, scrub) = match (&s.browser, &s.damaged) {
            (Some(b), _) => {
                let i = s.integrity.get(&(b.info.uuid, b.info.generation));
                (
                    i.and_then(|i| i.plan.as_ref()),
                    i.and_then(|i| i.scrub.as_ref()),
                )
            }
            (None, Some(d)) => (d.plan.as_ref(), None),
            _ => (None, None),
        };
        let _ = source;
        body = body.child(
            col()
                .w_full()
                .gap(10.0)
                .child(caption("First Aid", k))
                .child(
                    row()
                        .w_full()
                        .gap(12.0)
                        .child(health_tile(k, "Structure", plan, "Not checked yet"))
                        .child(health_tile(
                            k,
                            "File data",
                            scrub,
                            if plan.is_some_and(|p| !p.passed) {
                                "Checked once the structure is sound"
                            } else {
                                "Not verified yet"
                            },
                        )),
                ),
        );
    }
    let mut pairs: Vec<(&str, String)> = Vec::new();
    if let Some(b) = &s.browser {
        pairs.push(("Name", b.info.name()));
        pairs.push(("Format", format!("CarryFS, revision {}", b.info.revision)));
        pairs.push((
            "Location",
            match &b.device {
                Some(d) => format!(
                    "{} · {}",
                    d.id,
                    if d.internal { "Internal" } else { "External" }
                ),
                None => b.path.display().to_string(),
            },
        ));
        pairs.push(("Generation", b.info.generation.to_string()));
        pairs.push((
            "UUID",
            b.info
                .uuid
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        ));
        pairs.push(("Access", "Read-only".into()));
    } else if let Some(d) = &s.device {
        pairs.push(("Device", d.id.clone()));
        pairs.push(("Format", d.format_name().into()));
        pairs.push((
            "Mount point",
            d.mount
                .as_ref()
                .map_or_else(|| "Not mounted".into(), |p| p.display().to_string()),
        ));
        pairs.push((
            "Location",
            if d.internal { "Internal" } else { "External" }.into(),
        ));
        pairs.push((
            "Kind",
            match d.kind {
                DeviceKind::Physical => "Physical",
                DeviceKind::Virtual => "Virtual",
                DeviceKind::Unknown => "Not reported",
            }
            .into(),
        ));
        pairs.push((
            "UUID",
            d.uuid.clone().unwrap_or_else(|| "Not reported".into()),
        ));
    } else if let Some(d) = &s.damaged {
        pairs.push(("Location", d.source.path.display().to_string()));
        pairs.push(("Problem", d.error.clone()));
    }
    if !pairs.is_empty() {
        body = body.child(
            col()
                .w_full()
                .gap(10.0)
                .child(caption("Information", k))
                .child(facts(k, pairs)),
        );
    }
    if let Some(b) = &s.browser {
        let newer = b.attributes || b.preallocation || b.links;
        let mut features = col()
            .w_full()
            .gap(10.0)
            .child(caption("Format features", k))
            .child(facts(
                k,
                vec![
                    (
                        "Permissions",
                        if b.attributes {
                            "Stored, with the hidden flag".into()
                        } else {
                            "Not stored: files read as rw-r--r--".into()
                        },
                    ),
                    (
                        "Preallocation",
                        if b.preallocation {
                            "In use"
                        } else {
                            "Not used"
                        }
                        .into(),
                    ),
                    (
                        "Hard links",
                        if b.links { "In use" } else { "Not used" }.into(),
                    ),
                    (
                        "Compatibility",
                        if newer {
                            "Needs CarryFS newer than 0.1.0".into()
                        } else {
                            "Every CarryFS release".into()
                        },
                    ),
                ],
            ));
        if !b.attributes {
            features = features.child(
                row()
                    .gap(12.0)
                    .items_center()
                    .child(
                        Button::new("Upgrade Format…")
                            .disabled(busy)
                            .on_press(|s: &mut State| s.ask(SheetKind::Upgrade))
                            .build(k)
                            .key("upgrade"),
                    )
                    .child(muted(
                        "Store each file's permissions and hidden flag. Files are kept.",
                        k,
                    )),
            );
        }
        body = body.child(features);
    }
    body
}

// ---------------------------------------------------------------- files

pub(crate) fn recover_dialog(s: &mut State, tx: &mut Tx) {
    let Some(browser) = s.browser.clone() else {
        return;
    };
    tx.save_file(
        FileDialogOptions {
            title: "Recover this folder to a new destination".into(),
            file_name: Some("CarryFS recovered files".into()),
            ..Default::default()
        },
        move |s: &mut State, result, tx| match result {
            Ok(Some(path)) => {
                let browser = browser.clone();
                s.ask(SheetKind::Recover);
                if let Some(sheet) = &mut s.sheet {
                    sheet.destination = Some(path.clone());
                }
                s.run_sheet("Recover files", true, tx, move |cancel| {
                    operation::recover(browser, &path, cancel).map(Update::Report)
                });
            }
            Ok(None) => {}
            Err(e) => s.apply(Err(e.to_string())),
        },
    );
}

/// Ask for a new destination, then recover files to a folder or rebuild a new image.
pub(crate) fn fsck_dialog(s: &mut State, tx: &mut Tx, rebuild: bool) {
    let Some(source) = s.source() else {
        return;
    };
    tx.save_file(
        FileDialogOptions {
            title: if rebuild {
                "Rebuild as a new disk image"
            } else {
                "Recover files to a new folder"
            }
            .into(),
            file_name: Some(
                if rebuild {
                    "Recovered.cfs"
                } else {
                    "CarryFS recovered files"
                }
                .into(),
            ),
            ..Default::default()
        },
        move |s: &mut State, result, tx| match result {
            Ok(Some(path)) => {
                let source = source.clone();
                if s.sheet.is_none() {
                    s.ask(SheetKind::Recover);
                }
                if let Some(sheet) = &mut s.sheet {
                    sheet.destination = Some(path.clone());
                }
                let (name, mode) = if rebuild {
                    ("Rebuild as disk image", Fsck::Rebuild(path))
                } else {
                    ("Recover files", Fsck::Recover(path))
                };
                s.run_sheet(name, true, tx, move |cancel| {
                    operation::fsck(source, mode, cancel).map(Update::Report)
                });
            }
            Ok(None) => {}
            Err(e) => s.apply(Err(e.to_string())),
        },
    );
}

fn files(s: &State, k: &Kit) -> El {
    let Some(browser) = &s.browser else {
        return col().gap(12.0).child(label(
            if s.damaged.is_some() {
                "This volume's folders can't be read. Recover its files instead."
            } else {
                "Open the volume to browse its files. Unmounted CarryFS volumes open after you unlock."
            },
            k,
        ).wrap());
    };
    let busy = s.pending.is_some();
    let mut crumbs = row().items_center().gap(2.0);
    for (index, (_, name)) in browser.trail.iter().enumerate() {
        if index > 0 {
            crumbs = crumbs.child(muted("›", k));
        }
        let title = if index == 0 {
            browser.info.name()
        } else {
            name.clone()
        };
        crumbs = crumbs.child(
            Button::new(title.clone())
                .label_with(move |font, ink| text(title).font(font).color(ink).truncate())
                .variant(ButtonVariant::Ghost)
                .disabled(busy)
                .on_press_tx(move |s: &mut State, tx| {
                    if let Some(browser) = &s.browser {
                        s.navigate(browser.trail[..=index].to_vec(), tx);
                    }
                })
                .build(k)
                .key(format!("crumb-{index}")),
        );
    }
    let mut rows = col().w_full().gap(1.0);
    for (index, entry) in browser.entries.iter().enumerate() {
        let name = String::from_utf8_lossy(entry.item.name.stored()).into_owned();
        let size = if entry.item.kind == ObjectKind::Directory {
            "—".into()
        } else {
            bytes(entry.attrs.size)
        };
        let ink = if entry.attrs.hidden {
            k.c.text_weak
        } else {
            k.c.text_base
        };
        rows = rows.child(
            row()
                .key(format!("entry-{}", entry.item.object.get()))
                .w_full()
                .h(32.0)
                .px(10.0)
                .gap(10.0)
                .items_center()
                .role("button")
                .label(format!("{}: {name}", kind(entry.item.kind)))
                .focusable(!busy)
                .disabled(busy)
                .fill(if s.selected == Some(index) {
                    k.c.surface_hover
                } else {
                    k.c.bg_base
                })
                .radius(5.0)
                .hover(k.c.hover)
                .on_press(move |s: &mut State| {
                    if s.pending.is_none() {
                        s.selected = Some(index);
                    }
                })
                .on_double_press_tx(move |s: &mut State, tx| {
                    if s.pending.is_none() {
                        s.enter(index, tx);
                    }
                })
                .child(
                    icon(
                        match entry.item.kind {
                            ObjectKind::Directory => &FOLDER,
                            ObjectKind::File => &FILE,
                            ObjectKind::Symlink => &LINK,
                        },
                        16.0,
                    )
                    .color(k.c.icon_muted),
                )
                .child(
                    text(name)
                        .font(k.ty.body)
                        .color(ink)
                        .grow(1.0)
                        .min_w(0.0)
                        .truncate(),
                )
                .child(muted(kind(entry.item.kind), k).w(100.0))
                .child(muted(permissions(entry.attrs.mode), k).w(90.0))
                .child(muted(size, k).w(80.0)),
        );
    }
    if browser.entries.is_empty() {
        rows = rows.child(muted("This folder is empty.", k).padding(24.0));
    }
    let mut body = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .gap(12.0)
        .child(
            row()
                .w_full()
                .gap(8.0)
                .items_center()
                .child(
                    Button::new("Up")
                        .variant(ButtonVariant::Outline)
                        .disabled(busy || browser.trail.len() == 1)
                        .on_press_tx(|s: &mut State, tx| {
                            if let Some(browser) = &s.browser {
                                s.navigate(browser.trail[..browser.trail.len() - 1].to_vec(), tx);
                            }
                        })
                        .build(k)
                        .key("up"),
                )
                .child(el().grow(1.0).min_w(0.0).scroll_x().child(crumbs))
                .child(
                    Button::new("Recover This Folder…")
                        .disabled(busy)
                        .on_press_tx(recover_dialog)
                        .build(k)
                        .key("recover-folder"),
                ),
        )
        .child(muted(folder_summary(browser), k))
        .child(
            row()
                .px(10.0)
                .gap(10.0)
                .child(el().w(16.0))
                .child(muted("Name", k).grow(1.0))
                .child(muted("Kind", k).w(100.0))
                .child(muted("Permissions", k).w(90.0))
                .child(muted("Size", k).w(80.0)),
        )
        .child(rule(k))
        .child(
            col()
                .w_full()
                .key("file-list")
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(rows)
                .scroll_bar(k),
        );
    if let Some(index) = s.selected
        && let Some(entry) = browser.entries.get(index)
    {
        let folder = entry.item.kind == ObjectKind::Directory;
        let regular = entry.item.kind == ObjectKind::File;
        body = body.child(
            row()
                .w_full()
                .items_center()
                .gap(12.0)
                .padding(12.0)
                .radius(8.0)
                .fill(k.c.layer_01)
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(2.0)
                        .child(
                            label(
                                String::from_utf8_lossy(entry.item.name.stored()).into_owned(),
                                k,
                            )
                            .truncate(),
                        )
                        .child(muted(
                            format!(
                                "{} · {} · {} allocated · {}{}{}",
                                kind(entry.item.kind),
                                bytes(entry.attrs.size),
                                bytes(entry.attrs.allocated_bytes),
                                permissions(entry.attrs.mode),
                                if entry.attrs.hidden { " · Hidden" } else { "" },
                                if entry.attrs.links > 1 {
                                    format!(" · {} names", entry.attrs.links)
                                } else {
                                    String::new()
                                },
                            ),
                            k,
                        )),
                )
                .child(
                    Button::new(if folder { "Open Folder" } else { "Export…" })
                        .disabled(busy || !(folder || regular))
                        .on_press_tx(move |s: &mut State, tx| {
                            if folder {
                                s.enter(index, tx);
                                return;
                            }
                            if let Some(browser) = s.browser.clone()
                                && let Some(entry) = browser.entries.get(index)
                            {
                                let object = entry.item.object;
                                let file_name =
                                    String::from_utf8_lossy(entry.item.name.stored()).into_owned();
                                tx.save_file(
                                    FileDialogOptions {
                                        title: "Export file to a new path".into(),
                                        file_name: Some(file_name),
                                        ..Default::default()
                                    },
                                    move |s: &mut State, result, tx| match result {
                                        Ok(Some(path)) => {
                                            let browser = browser.clone();
                                            s.start("Export file", false, tx, move |_| {
                                                browser.export(object, &path).map(Update::Report)
                                            });
                                        }
                                        Ok(None) => {}
                                        Err(e) => s.apply(Err(e.to_string())),
                                    },
                                );
                            }
                        })
                        .build(k)
                        .key("selection-action"),
                ),
        );
    }
    body
}

/// Counts and sizes of the listed folder's direct entries.
pub(crate) fn folder_summary(browser: &Browser) -> String {
    let count = |wanted| {
        browser
            .entries
            .iter()
            .filter(|e| e.item.kind == wanted)
            .count()
    };
    let size: u64 = browser
        .entries
        .iter()
        .filter(|e| e.item.kind == ObjectKind::File)
        .map(|e| e.attrs.size)
        .sum();
    let allocated: u64 = browser
        .entries
        .iter()
        .map(|e| e.attrs.allocated_bytes)
        .sum();
    let hidden = browser.entries.iter().filter(|e| e.attrs.hidden).count();
    let mut summary = format!(
        "Folders {} · Files {} · Symlinks {} · {} in files · {} allocated",
        count(ObjectKind::Directory),
        count(ObjectKind::File),
        count(ObjectKind::Symlink),
        bytes(size),
        bytes(allocated)
    );
    if hidden > 0 {
        summary += &format!(" · Hidden {hidden}");
    }
    summary
}

// ---------------------------------------------------------------- history

fn activity(s: &State, k: &Kit) -> El {
    let mut body = col().w_full().gap(0.0);
    if s.history.is_empty() {
        return label(
            "Tasks you run appear here with their disk, result, and time.",
            k,
        )
        .wrap();
    }
    for (index, record) in s.history.iter().rev().enumerate() {
        let seconds = record
            .at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if index > 0 {
            body = body.child(rule(k));
        }
        body = body.child(
            row()
                .w_full()
                .gap(12.0)
                .py(12.0)
                .items_start()
                .child(
                    icon(if record.failed { &WARNING } else { &OK }, 18.0).color(
                        if record.failed {
                            k.c.warning.icon
                        } else {
                            k.c.success.icon
                        },
                    ),
                )
                .child(
                    col()
                        .grow(1.0)
                        .min_w(0.0)
                        .gap(3.0)
                        .child(
                            row()
                                .gap(8.0)
                                .child(
                                    text(record.task.clone())
                                        .font(k.ty.ui_medium)
                                        .color(k.c.text_base),
                                )
                                .child(muted(&record.target, k).truncate()),
                        )
                        .child(muted(&record.message, k).wrap_anywhere()),
                )
                .child(muted(
                    format!(
                        "{:02}:{:02}:{:02} UTC",
                        seconds / 3600 % 24,
                        seconds / 60 % 60,
                        seconds % 60
                    ),
                    k,
                )),
        );
    }
    body
}

// ---------------------------------------------------------------- software

fn software(s: &State, k: &Kit) -> El {
    let busy = s.pending.is_some();
    let core = include_str!("../../Cargo.toml")
        .lines()
        .find_map(|s| {
            s.strip_prefix("version = \"")
                .and_then(|s| s.strip_suffix('"'))
        })
        .unwrap_or("Unknown");
    let installed = s.versions.installed.clone();
    let mut actions = row().gap(8.0).child(
        Button::new("Check for Updates")
            .disabled(busy)
            .on_press_tx(|s: &mut State, tx| {
                s.start("Check for updates", false, tx, |_| {
                    platform::check_updates().map(Update::Versions)
                })
            })
            .build(k)
            .key("check-updates"),
    );
    if let Some(latest) = s
        .versions
        .latest
        .clone()
        .filter(|_| s.versions.update_available())
    {
        actions = actions.child(
            Button::new(if installed.is_some() {
                format!("Update to {latest}…")
            } else {
                format!("Install CarryFS {latest}…")
            })
            .variant(ButtonVariant::Submit)
            .disabled(busy)
            .on_press(move |s: &mut State| s.ask(SheetKind::Install(latest.clone())))
            .build(k)
            .key("install-update"),
        );
    }
    col()
        .w_full()
        .padding(24.0)
        .gap(20.0)
        .child(heading("Software Update", k))
        .child(
            label(
                match (&installed, s.versions.update_available(), &s.versions.latest) {
                    (None, _, _) => "CarryFS isn't installed on this Mac. Install it to mount CarryFS disks in Finder.".to_owned(),
                    (Some(_), true, Some(latest)) => format!("CarryFS {latest} is available."),
                    (Some(version), _, Some(_)) => format!("CarryFS {version} is up to date."),
                    (Some(version), _, None) => format!("CarryFS {version} is installed. Check for updates to see newer releases."),
                },
                k,
            )
            .wrap(),
        )
        .child(actions)
        .child(facts(
            k,
            vec![
                ("Installed CarryFS", installed.unwrap_or_else(|| "Not installed".into())),
                (
                    "Latest release",
                    s.versions.latest.clone().unwrap_or_else(|| "Not checked".into()),
                ),
                ("CarryFS Manager", env!("CARGO_PKG_VERSION").into()),
                ("Bundled tools", core.into()),
            ],
        ))
        .child(
            muted(
                "Updating CarryFS replaces the software only; it never changes your disks. Eject CarryFS volumes before updating. Newer format features, such as stored permissions, are added per volume with Upgrade Format, and CarryFS 0.1.0 can't mount volumes that use them.",
                k,
            )
            .wrap(),
        )
}

// ---------------------------------------------------------------- sheets

fn sheet(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(sheet) = &s.sheet else {
        return;
    };
    let busy = s.pending.is_some();
    let name = s
        .browser
        .as_ref()
        .map(|b| b.info.name())
        .or_else(|| s.damaged.as_ref().map(|d| d.source.name()))
        .or_else(|| s.device.as_ref().map(|d| d.name.clone()))
        .unwrap_or_default();
    let cancel = || {
        Button::new("Cancel")
            .variant(ButtonVariant::Outline)
            .on_press(|s: &mut State| s.sheet = None)
            .build(k)
            .key("cancel-sheet")
    };
    let buttons = |items: Vec<El>| row().w_full().gap(8.0).justify_end().children(items);
    let (title, description, body): (String, String, El) = match sheet.phase {
        Phase::Running => (
            format!("{}…", s.task),
            name.clone(),
            col()
                .w_full()
                .gap(12.0)
                .child(Progress::new(None).label(format!("{} · {} s", s.task, s.elapsed_seconds)).build(k))
                .child(buttons(vec![
                    Button::new("Stop")
                        .variant(ButtonVariant::Outline)
                        .disabled(s.cancel.is_none())
                        .on_press(|s: &mut State| {
                            if let Some(cancel) = &s.cancel {
                                cancel.cancel();
                                s.status = "Stopping…".into();
                            }
                        })
                        .build(k)
                        .key("cancel-operation"),
                ])),
        ),
        Phase::Done => done(s, sheet, &name, k),
        Phase::Ask => match &sheet.kind {
            SheetKind::FirstAid => (
                format!("Run First Aid on “{name}”?"),
                "First Aid checks the volume's structure and, if it's sound, verifies every file's data. Nothing is changed.".into(),
                buttons(vec![
                    cancel(),
                    Button::new("Run")
                        .variant(ButtonVariant::Submit)
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.first_aid(tx))
                        .build(k)
                        .key("confirm-first-aid"),
                ]),
            ),
            SheetKind::Erase => {
                let device = s.device.as_ref();
                let valid = carryfs_core::VolumeLabel::parse(s.erase_name.text.trim()).is_ok();
                (
                    format!("Erase “{name}”?"),
                    format!(
                        "Erasing “{name}” permanently deletes all data stored on it. You can't undo this."
                    ),
                    col()
                        .w_full()
                        .gap(14.0)
                        .child(
                            TextInput::new(&s.erase_name)
                                .label("Name")
                                .placeholder("CarryFS")
                                .on_edit(|s: &mut State| &mut s.erase_name)
                                .build(k)
                                .key("erase-name"),
                        )
                        .child(facts(
                            k,
                            vec![
                                ("Format", "CarryFS".into()),
                                (
                                    "Partition",
                                    device.map_or_else(String::new, |d| {
                                        format!("{} · {}", d.id, bytes(d.bytes))
                                    }),
                                ),
                            ],
                        ))
                        .child(buttons(vec![
                            cancel(),
                            Button::new("Erase")
                                .variant(ButtonVariant::Danger)
                                .disabled(busy || !valid)
                                .on_press_tx(|s: &mut State, tx| s.erase(tx))
                                .build(k)
                                .key("confirm-erase"),
                        ])),
                )
            }
            SheetKind::Eject => (
                format!("Eject “{name}”?"),
                "Every volume on this disk will be unmounted and ejected. Close files on it first.".into(),
                buttons(vec![
                    cancel(),
                    Button::new("Eject")
                        .variant(ButtonVariant::Submit)
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.device_action(DeviceAction::Eject, tx))
                        .build(k)
                        .key("confirm-eject"),
                ]),
            ),
            SheetKind::Upgrade => (
                format!("Upgrade “{name}”?"),
                "The volume will store each file's permissions and hidden flag. Your files are kept. CarryFS 0.1.0 can't mount it afterwards, and the upgrade can't be undone, so update CarryFS on every computer that uses this volume first.".into(),
                buttons(vec![
                    cancel(),
                    Button::new("Upgrade")
                        .variant(ButtonVariant::Submit)
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.upgrade(tx))
                        .build(k)
                        .key("confirm-upgrade"),
                ]),
            ),
            SheetKind::Repair => (
                format!("Repair “{name}”?"),
                "First Aid rebuilds the damaged metadata in place. File data isn't rewritten. To keep a copy of the volume as it is now, recover its files first.".into(),
                buttons(vec![
                    cancel(),
                    Button::new("Repair")
                        .variant(ButtonVariant::Submit)
                        .disabled(busy)
                        .on_press_tx(|s: &mut State, tx| s.repair(tx))
                        .build(k)
                        .key("confirm-repair"),
                ]),
            ),
            SheetKind::Recover => {
                let option = |title: &str, detail: &str, action: &str, key: &str, rebuild: bool| {
                    row()
                        .w_full()
                        .gap(12.0)
                        .padding(14.0)
                        .radius(8.0)
                        .border(1.0, k.c.border_weak)
                        .items_center()
                        .child(
                            col()
                                .grow(1.0)
                                .min_w(0.0)
                                .gap(2.0)
                                .child(text(title.to_owned()).font(k.ty.ui_medium).color(k.c.text_base))
                                .child(muted(detail.to_owned(), k).wrap()),
                        )
                        .child(
                            Button::new(action.to_owned())
                                .disabled(busy)
                                .on_press_tx(move |s: &mut State, tx| fsck_dialog(s, tx, rebuild))
                                .build(k)
                                .key(key.to_owned()),
                        )
                };
                (
                    format!("Recover Files from “{name}”"),
                    "Copies everything readable to a new place. The volume itself isn't changed. Parts that can't be read are listed in a report beside the copy.".into(),
                    col()
                        .w_full()
                        .gap(10.0)
                        .child(option(
                            "Copy to a folder",
                            "Files keep their permissions, hidden flags, and hard links.",
                            "Choose Folder…",
                            "recover",
                            false,
                        ))
                        .child(option(
                            "Rebuild as a disk image",
                            "A new, verified CarryFS image with the same files.",
                            "Choose Location…",
                            "rebuild",
                            true,
                        ))
                        .child(buttons(vec![cancel()])),
                )
            }
            SheetKind::Install(version) => {
                let version = version.clone();
                (
                    format!("Install CarryFS {version}?"),
                    "Eject all CarryFS volumes first. The package is verified before it's installed, and you'll be asked for your password once. Your disks aren't changed.".into(),
                    buttons(vec![
                        cancel(),
                        Button::new("Install")
                            .variant(ButtonVariant::Submit)
                            .disabled(busy)
                            .on_press_tx(move |s: &mut State, tx| s.install(version.clone(), tx))
                            .build(k)
                            .key("confirm-install"),
                    ]),
                )
            }
        },
    };
    // The kit's description line doesn't wrap, so the text leads the body.
    let body = col()
        .w_full()
        .gap(16.0)
        .child(muted(description, k).wrap())
        .child(body);
    Dialog::new("sheet", title)
        .width(540.0)
        .fit()
        .click_outside(false)
        .open(true)
        .on_close(|s: &mut State| {
            if s.sheet
                .as_ref()
                .is_some_and(|sheet| sheet.phase != Phase::Running)
            {
                s.sheet = None;
            }
        })
        .child(body.px(16.0).pb(16.0))
        .build(k, layer);
}

/// A finished sheet: the outcome, its details, and what to do next.
fn done(s: &State, sheet: &Sheet, name: &str, k: &Kit) -> (String, String, El) {
    let (ok, message) = sheet.outcome.clone().unwrap_or((false, String::new()));
    let finish = Button::new("Done")
        .variant(ButtonVariant::Submit)
        .on_press(|s: &mut State| s.sheet = None)
        .build(k)
        .key("close-sheet");
    let mut actions = Vec::new();
    let (title, details) = match &sheet.kind {
        SheetKind::FirstAid => {
            let (plan, scrub) = match (&s.browser, &s.damaged) {
                (Some(b), _) => {
                    let i = s.integrity.get(&(b.info.uuid, b.info.generation));
                    (
                        i.and_then(|i| i.plan.clone()),
                        i.and_then(|i| i.scrub.clone()),
                    )
                }
                (None, Some(d)) => (d.plan.clone(), None),
                _ => (None, None),
            };
            let mut details: Vec<String> = plan
                .iter()
                .flat_map(|p| p.summary.lines())
                .filter(|line| !line.starts_with("File data checksums have not"))
                .map(str::to_owned)
                .collect();
            details.extend(
                scrub
                    .iter()
                    .flat_map(|r| r.summary.lines().map(str::to_owned)),
            );
            if !ok {
                actions.push(
                    Button::new("Recover Files…")
                        .on_press(|s: &mut State| s.ask(SheetKind::Recover))
                        .build(k)
                        .key("first-aid-recover"),
                );
                let repairable = plan.as_ref().is_some_and(ScanResult::repairable)
                    && s.source().is_some_and(|source| source.device.is_none());
                if repairable {
                    actions.push(
                        Button::new("Repair…")
                            .on_press(|s: &mut State| s.ask(SheetKind::Repair))
                            .build(k)
                            .key("repair"),
                    );
                }
            }
            (
                if ok {
                    format!("First Aid found no problems on “{name}”.")
                } else {
                    format!("First Aid found problems on “{name}”.")
                },
                details.join("\n"),
            )
        }
        kind => {
            if ok
                && matches!(kind, SheetKind::Recover)
                && let Some(destination) = sheet.destination.clone()
            {
                actions.push(
                    Button::new("Show in Finder")
                        .on_press(move |s: &mut State| {
                            if let Err(error) = platform::reveal(&destination) {
                                s.apply(Err(error.to_string()));
                            }
                        })
                        .build(k)
                        .key("reveal"),
                );
            }
            let verb = match kind {
                SheetKind::Erase => "Erase",
                SheetKind::Eject => "Eject",
                SheetKind::Upgrade => "Upgrade",
                SheetKind::Repair => "Repair",
                SheetKind::Recover => "Recovery",
                SheetKind::Install(_) => "Installation",
                SheetKind::FirstAid => "First Aid",
            };
            (
                if ok {
                    format!("{verb} finished.")
                } else {
                    format!("{verb} didn't finish.")
                },
                message.clone(),
            )
        }
    };
    actions.push(finish);
    let body = col()
        .w_full()
        .gap(12.0)
        .child(
            row()
                .gap(10.0)
                .items_center()
                .child(icon(if ok { &OK } else { &WARNING }, 22.0).color(if ok {
                    k.c.success.icon
                } else {
                    k.c.warning.icon
                }))
                .child(label(title.clone(), k).wrap().grow(1.0).min_w(0.0)),
        )
        .child(
            col()
                .w_full()
                .max_h(240.0)
                .padding(10.0)
                .radius(6.0)
                .fill(k.c.layer_01)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(
                    text(details)
                        .font(k.ty.mono)
                        .color(k.c.text_weak)
                        .wrap_anywhere(),
                )
                .scroll_bar(k),
        )
        .child(row().w_full().gap(8.0).justify_end().children(actions));
    (
        if ok {
            "Finished".into()
        } else {
            "Needs attention".into()
        },
        name.to_owned(),
        body,
    )
}

// ---------------------------------------------------------------- status

fn status_bar(s: &State, k: &Kit) -> El {
    let mut bar = row()
        .w_full()
        .h(30.0)
        .px(16.0)
        .gap(12.0)
        .items_center()
        .fill(k.c.layer_01);
    if s.pending.is_some() && s.sheet.is_none() {
        bar = bar
            .child(muted(format!("{} · {} s", s.task, s.elapsed_seconds), k))
            .child(
                Button::new("Stop")
                    .variant(ButtonVariant::GhostMuted)
                    .disabled(s.cancel.is_none())
                    .on_press(|s: &mut State| {
                        if let Some(cancel) = &s.cancel {
                            cancel.cancel();
                            s.status = "Stopping…".into();
                        }
                    })
                    .build(k)
                    .key("cancel-operation"),
            );
    }
    bar.child(
        text(
            s.status
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(220)
                .collect::<String>(),
        )
        .key("status")
        .role(if s.error { "alert" } else { "status" })
        .live_region(LiveRegion::Polite)
        .font(k.ty.text_small)
        .color(if s.error {
            k.c.danger.icon
        } else {
            k.c.text_weak
        })
        .grow(1.0)
        .min_w(0.0)
        .truncate(),
    )
}
