//! Fallback for platforms without a per-application mixer backend yet (macOS).
//!
//! macOS has no public API to change another application's volume. A future backend
//! could use Core Audio process taps (macOS 14.2+) or a virtual audio driver.

use super::{AppStream, AudioBackend, AudioError, AudioResult, OutputDevice, Volume};

pub struct UnsupportedBackend;

impl AudioBackend for UnsupportedBackend {
	fn apps(&self) -> AudioResult<Vec<AppStream>> {
		Err(AudioError::Unsupported)
	}

	fn outputs(&self) -> AudioResult<Vec<OutputDevice>> {
		Err(AudioError::Unsupported)
	}

	fn set_volume(&self, _app_key: &str, _volume: Volume) -> AudioResult<()> {
		Err(AudioError::Unsupported)
	}

	fn set_muted(&self, _app_key: &str, _muted: bool) -> AudioResult<()> {
		Err(AudioError::Unsupported)
	}

	fn move_to_output(&self, _app_key: &str, _output_id: &str) -> AudioResult<()> {
		Err(AudioError::Unsupported)
	}

	fn max_volume(&self) -> Volume {
		1.0
	}
}
