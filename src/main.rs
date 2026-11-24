mod action;
mod audio;
mod config;
mod dial;
mod i18n;
mod icons;
mod names;
mod render;
mod state;

use std::time::{Duration, Instant};

use openaction::*;

/// Fallback refresh period, for changes the backend does not report (and for Windows).
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// Resend every dial's whole screen this often, in case the host lost it.
const FULL_REDRAW_INTERVAL: Duration = Duration::from_secs(30);
/// Wait this long after a change event so bursts of events cause one refresh.
const EVENT_DEBOUNCE: Duration = Duration::from_millis(50);

/// `appvolume --list`: print what the plugin sees, to check audio and icon access by hand.
fn print_diagnostics() {
	let backend = audio::backend();
	match backend.apps() {
		Ok(apps) if apps.is_empty() => println!("No application is playing audio."),
		Ok(apps) => {
			for app in apps {
				let icon = match icons::icon_file(&app.icon) {
					Some(path) => format!("icon {}", path.display()),
					None if icons::app_icon(&app.icon) == icons::fallback_icon() => {
						"no icon found".to_owned()
					}
					None => "icon found".to_owned(),
				};
				println!(
					"{} [{}]: {:.0}%{}, output {}, {icon} (tried {:?}), reports {:?}",
					audio::display_name(&app.key, &app.name),
					app.key,
					app.volume * 100.0,
					if app.muted { " (muted)" } else { "" },
					app.output_id.as_deref().unwrap_or("?"),
					app.icon.names,
					app.reported,
				);
			}
		}
		Err(error) => println!("Applications: {error}"),
	}
	match backend.outputs() {
		Ok(outputs) => {
			for output in outputs {
				println!("Output: {} [{}]", output.name, output.id);
			}
		}
		Err(error) => println!("Outputs: {error}"),
	}
}

/// Plugin-wide events: the global settings edited in the property inspector.
struct GlobalHandler;

#[async_trait]
impl global_events::GlobalEventHandler for GlobalHandler {
	async fn plugin_ready(&self) -> OpenActionResult<()> {
		get_global_settings().await
	}

	async fn did_receive_global_settings(
		&self,
		event: global_events::DidReceiveGlobalSettingsEvent,
	) -> OpenActionResult<()> {
		config::set(&event.payload.settings);
		// Language or maximum volume may have changed: send every screen again.
		state::forget_all_sent();
		state::redraw_all().await;
		Ok(())
	}
}

#[tokio::main]
async fn main() -> OpenActionResult<()> {
	let args: Vec<String> = std::env::args().collect();
	if args.iter().any(|a| a == "--list") {
		i18n::init(&args);
		print_diagnostics();
		return Ok(());
	}

	{
		use simplelog::*;
		// The logger accepts Debug; the "Debug logs" setting moves the global max level
		// (config::apply_log_level). Only this plugin and the OpenAction crate are logged:
		// dependencies are too chatty.
		let filter = ConfigBuilder::new()
			.add_filter_allow_str("appvolume")
			.add_filter_allow_str("openaction")
			.build();
		if let Err(error) = TermLogger::init(
			LevelFilter::Debug,
			filter,
			TerminalMode::Stdout,
			ColorChoice::Never,
		) {
			eprintln!("Logger initialization failed: {error}");
		}
		config::apply_log_level(config::DEBUG_DEFAULT);
		i18n::init(&args);
		log::info!("AppVolume {} started", env!("CARGO_PKG_VERSION"));
	}

	let plugin = &*state::PLUGIN;
	let changed = plugin.changed.clone();
	plugin
		.backend
		.watch(std::sync::Arc::new(move || changed.notify_one()));

	tokio::spawn(async {
		let mut last_full_redraw = Instant::now();
		loop {
			state::refresh().await;
			dial::expire_output_modes();
			if last_full_redraw.elapsed() > FULL_REDRAW_INTERVAL {
				state::forget_all_sent();
				last_full_redraw = Instant::now();
			}
			state::redraw_all().await;

			tokio::select! {
				_ = state::PLUGIN.changed.notified() => tokio::time::sleep(EVENT_DEBOUNCE).await,
				_ = tokio::time::sleep(REFRESH_INTERVAL) => {}
			}
		}
	});

	register_action(action::AppVolumeAction).await;
	global_events::set_global_event_handler(&GlobalHandler);

	run(args).await
}
