//! Top-level command → handler dispatch.

use std::io::Write;

use crate::cli::{
    AdminSub, BtSub, Cli, Command, EthSub, GnssSub, IfaceSub, PowerSub, ProfileSub, WifiSub,
};
use crate::commands;
use crate::errors::NexusctlError;
use crate::proxy::{BluetoothListFilter, ManagerOps};

pub async fn dispatch(
    cli: &Cli,
    ops: &dyn ManagerOps,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let format = cli.global.output_format();
    let ctx = cli.global.render_context();
    match &cli.command {
        None => commands::status::run(ops, format, &ctx, stdout).await,

        Some(Command::Status) => commands::status::run(ops, format, &ctx, stdout).await,

        Some(Command::Iface { sub }) => match sub {
            IfaceSub::List { kind } => {
                commands::iface::list(ops, kind.map(|k| k.as_wire()), format, &ctx, stdout).await
            }
            IfaceSub::Show { iface } => {
                commands::iface::show(ops, iface, None, format, &ctx, stdout).await
            }
            IfaceSub::Events { iface: _ } => commands::iface::events_stub(stdout),
        },

        Some(Command::Eth { sub }) => match sub {
            EthSub::List => {
                commands::iface::list(ops, Some("ethernet"), format, &ctx, stdout).await
            }
            EthSub::Show { iface } => {
                commands::iface::show(ops, iface, Some("ethernet"), format, &ctx, stdout).await
            }
        },

        Some(Command::Wifi { sub }) => match sub {
            WifiSub::List => commands::iface::list(ops, Some("wifi"), format, &ctx, stdout).await,
            WifiSub::Show { iface } => {
                commands::iface::show_wifi(ops, iface.as_deref(), format, &ctx, stdout).await
            }
            WifiSub::Scan { iface } => {
                commands::wifi::scan(ops, iface.as_deref(), format, &ctx, stdout).await
            }
            WifiSub::Connect {
                ssid,
                iface,
                psk,
                no_warn_psk,
            } => {
                commands::wifi::connect(
                    ops,
                    ssid,
                    iface.as_deref(),
                    psk.as_deref(),
                    *no_warn_psk,
                    stderr,
                    format,
                    &ctx,
                    stdout,
                )
                .await
            }
            WifiSub::ConnectProfile { profile, iface } => {
                commands::wifi::connect_profile(
                    ops,
                    profile,
                    iface.as_deref(),
                    format,
                    &ctx,
                    stdout,
                )
                .await
            }
            WifiSub::Disconnect { iface } => {
                commands::wifi::disconnect(ops, iface.as_deref(), format, &ctx, stdout).await
            }
            WifiSub::Forget { reference } => {
                commands::wifi::forget(ops, reference, format, &ctx, stdout).await
            }
        },

        Some(Command::Bt { sub }) => match sub {
            BtSub::Adapters => commands::bt::adapters(ops, format, &ctx, stdout).await,
            BtSub::List { paired, connected } => {
                let filter = if *paired {
                    BluetoothListFilter::Paired
                } else if *connected {
                    BluetoothListFilter::Connected
                } else {
                    BluetoothListFilter::All
                };
                commands::bt::list(ops, filter, format, &ctx, stdout).await
            }
            BtSub::Show { address } => commands::bt::show(ops, address, format, &ctx, stdout).await,
            BtSub::Power { hci, state } => {
                commands::bt_mutating::power(ops, hci, state.as_bool(), format, &ctx, stdout).await
            }
            BtSub::Scan { hci, duration } => {
                commands::bt_mutating::scan(ops, hci.as_deref(), *duration, format, &ctx, stdout)
                    .await
            }
            BtSub::Connect { address } => {
                commands::bt_mutating::connect(ops, address, format, &ctx, stdout).await
            }
            BtSub::Disconnect { address } => {
                commands::bt_mutating::disconnect(ops, address, format, &ctx, stdout).await
            }
            BtSub::Forget { address } => {
                commands::bt_mutating::forget(ops, address, format, &ctx, stdout).await
            }
            BtSub::Trust { address, state } => {
                commands::bt_mutating::trust(ops, address, state.as_bool(), format, &ctx, stdout)
                    .await
            }
            BtSub::Pair { address, timeout } => {
                commands::bt_mutating::pair(
                    ops,
                    address,
                    std::time::Duration::from_secs(*timeout),
                    crate::interactive::terminal_prompt::TerminalPrompt::new(),
                    format,
                    &ctx,
                    stdout,
                )
                .await
            }
        },

        Some(Command::Gnss { sub }) => match sub {
            GnssSub::List => commands::iface::list(ops, Some("gnss"), format, &ctx, stdout).await,
            GnssSub::Show { device } => {
                commands::gnss::show(ops, device.as_deref(), format, &ctx, stdout).await
            }
            GnssSub::Satellites { device } => {
                commands::gnss::satellites(ops, device.as_deref(), format, &ctx, stdout).await
            }
        },

        Some(Command::Profile { sub }) => match sub {
            ProfileSub::List { kind } => {
                commands::profile::list(ops, kind.map(|k| k.as_wire()), format, &ctx, stdout).await
            }
            ProfileSub::Show { reference } => {
                commands::profile::show(ops, reference, format, &ctx, stdout).await
            }
            ProfileSub::Export { reference } => {
                commands::profile::export(ops, reference, stdout).await
            }
            ProfileSub::AddWifi {
                ssid,
                psk,
                file,
                label,
                priority,
                auto_connect,
                hidden,
                fast_transition,
                security,
                no_warn_psk,
            } => {
                commands::profile_mutating::add_wifi(
                    ops,
                    ssid.as_deref(),
                    psk.as_deref(),
                    file.as_deref(),
                    label.as_deref(),
                    *priority,
                    *auto_connect,
                    *hidden,
                    *fast_transition,
                    security,
                    *no_warn_psk,
                    stderr,
                    format,
                    &ctx,
                    stdout,
                )
                .await
            }
            ProfileSub::AddEthernet {
                ifname,
                file,
                label,
                auto_connect,
            } => {
                commands::profile_mutating::add_ethernet(
                    ops,
                    ifname.as_deref(),
                    file.as_deref(),
                    label.as_deref(),
                    *auto_connect,
                    format,
                    &ctx,
                    stdout,
                )
                .await
            }
            ProfileSub::Import { kind, file } => {
                commands::profile_mutating::import(
                    ops,
                    *kind,
                    file.as_deref(),
                    format,
                    &ctx,
                    stdout,
                )
                .await
            }
            ProfileSub::Remove { reference } => {
                commands::profile_mutating::remove(ops, reference, format, &ctx, stdout).await
            }
            ProfileSub::Update {
                reference,
                field,
                value,
            } => {
                commands::profile_mutating::update(
                    ops, reference, field, value, format, &ctx, stdout,
                )
                .await
            }
        },

        Some(Command::Power { sub }) => match sub {
            PowerSub::Get => commands::power::get(ops, format, &ctx, stdout).await,
            PowerSub::Set { state } => {
                commands::power::set(ops, state.as_wire(), format, &ctx, stdout).await
            }
        },

        Some(Command::Admin { sub }) => match sub {
            AdminSub::MasterKeyInfo => {
                commands::admin::master_key_info(ops, format, &ctx, stdout).await
            }
            AdminSub::RotateMasterKey => {
                commands::admin::rotate_master_key(ops, format, &ctx, stdout).await
            }
            AdminSub::FreezeBackup => {
                commands::admin::freeze_backup(ops, format, &ctx, stdout).await
            }
            AdminSub::ReleaseBackup { lease } => {
                commands::admin::release_backup(ops, lease, format, &ctx, stdout).await
            }
            AdminSub::Diagnostics { out } => {
                commands::admin::diagnostics_stub(out.as_deref(), stdout)
            }
            AdminSub::ReloadConfig => {
                commands::admin::reload_config(ops, format, &ctx, stdout).await
            }
        },
    }
}
