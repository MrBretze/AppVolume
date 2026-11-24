//! Windows backend: WASAPI audio sessions for volume, and the undocumented
//! `IAudioPolicyConfigFactory` (the API behind Settings › Sound › App volume) for
//! routing an app to another output.
//!
//! COM is initialised (multithreaded) on every call, because calls come from
//! arbitrary `spawn_blocking` threads.

use super::{
	AppStream, AudioBackend, AudioError, AudioResult, IconHint, OutputDevice, SYSTEM_KEY, Volume,
	group_streams,
};

use crate::names::{self, Mapped};

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Media::Audio::{
	AudioSessionStateExpired, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioSessionControl2,
	IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
	eConsole, eMultimedia, eRender,
};
use windows::Win32::Storage::FileSystem::{
	GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
	CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{
	OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::core::{GUID, HRESULT, HSTRING, IInspectable, IUnknown, Interface, PCWSTR, PWSTR};

pub struct WasapiBackend;

impl WasapiBackend {
	pub fn new() -> Self {
		Self
	}
}

/// One WASAPI session, with what is needed to act on it.
struct Session {
	pid: u32,
	device_id: String,
	exe: Option<PathBuf>,
	display_name: Option<String>,
	volume: ISimpleAudioVolume,
	/// The Windows "System sounds" session (process id 0).
	system: bool,
}

impl Session {
	fn key(&self) -> String {
		if self.system || self.mapping() == Some(Mapped::System) {
			SYSTEM_KEY.to_owned()
		} else {
			app_key(self.exe.as_deref(), self.pid)
		}
	}

	/// Names matched against `app-names.json`: executable name, then session name.
	fn reported(&self) -> Vec<String> {
		let stem = self
			.exe
			.as_deref()
			.and_then(|p| p.file_stem())
			.map(|n| n.to_string_lossy().into_owned());
		[stem, self.display_name.clone()]
			.into_iter()
			.flatten()
			.collect()
	}

	fn mapping(&self) -> Option<Mapped> {
		let reported = self.reported();
		names::lookup(reported.iter().map(String::as_str))
	}
}

impl AudioBackend for WasapiBackend {
	fn apps(&self) -> AudioResult<Vec<AppStream>> {
		let streams = sessions()?
			.into_iter()
			.map(|s| {
				let mapped = match s.mapping() {
					Some(Mapped::Name(name)) => Some(name),
					_ => None,
				};
				let name = mapped
					.or_else(|| s.display_name.clone())
					.or_else(|| s.exe.as_deref().and_then(file_description))
					.or_else(|| {
						s.exe
							.as_deref()
							.and_then(|p| p.file_stem())
							.map(|n| n.to_string_lossy().into_owned())
					})
					.unwrap_or_else(|| format!("PID {}", s.pid));
				// SAFETY: COM calls on a valid interface pointer.
				let volume = unsafe { s.volume.GetMasterVolume() }.unwrap_or(0.0);
				let muted = unsafe { s.volume.GetMute() }
					.map(|b| b.as_bool())
					.unwrap_or(false);
				AppStream {
					key: s.key(),
					name,
					icon: IconHint {
						names: Vec::new(),
						exe_path: s.exe.clone(),
					},
					volume,
					muted,
					output_id: Some(s.device_id.clone()),
					reported: s.reported(),
				}
			})
			.collect();
		Ok(group_streams(streams))
	}

	fn outputs(&self) -> AudioResult<Vec<OutputDevice>> {
		let mut outputs = Vec::new();
		for device in render_devices()? {
			let id = device_id(&device)?;
			let name = device_name(&device).unwrap_or_else(|| id.clone());
			outputs.push(OutputDevice { id, name });
		}
		Ok(outputs)
	}

	fn set_volume(&self, app_key: &str, volume: Volume) -> AudioResult<()> {
		let matching: Vec<_> = sessions()?
			.into_iter()
			.filter(|s| s.key() == app_key)
			.collect();
		if matching.is_empty() {
			return Err(AudioError::NotFound);
		}
		for session in matching {
			// SAFETY: COM call on a valid interface pointer; a null event context is allowed.
			unsafe {
				session
					.volume
					.SetMasterVolume(volume.clamp(0.0, 1.0), std::ptr::null())
			}
			.map_err(win_error)?;
		}
		Ok(())
	}

	fn set_muted(&self, app_key: &str, muted: bool) -> AudioResult<()> {
		let matching: Vec<_> = sessions()?
			.into_iter()
			.filter(|s| s.key() == app_key)
			.collect();
		if matching.is_empty() {
			return Err(AudioError::NotFound);
		}
		for session in matching {
			// SAFETY: COM call on a valid interface pointer; no event context.
			unsafe { session.volume.SetMute(muted, std::ptr::null()) }.map_err(win_error)?;
		}
		Ok(())
	}

	fn move_to_output(&self, app_key: &str, output_id: &str) -> AudioResult<()> {
		let mut pids: Vec<u32> = sessions()?
			.into_iter()
			.filter(|s| s.key() == app_key && !s.system)
			.map(|s| s.pid)
			.collect();
		pids.sort_unstable();
		pids.dedup();
		if pids.is_empty() {
			return Err(AudioError::NotFound);
		}
		let policy = PolicyConfig::new()?;
		let device = HSTRING::from(policy_device_id(output_id));
		for pid in pids {
			for role in [eConsole, eMultimedia] {
				policy.set_persisted_default_endpoint(pid, eRender, role, &device)?;
			}
		}
		Ok(())
	}

	fn max_volume(&self) -> Volume {
		1.0
	}
}

fn init_com() {
	// SAFETY: plain COM initialisation; S_FALSE / RPC_E_CHANGED_MODE just mean it is already initialised.
	let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

fn win_error(error: windows::core::Error) -> AudioError {
	AudioError::Other(error.to_string())
}

fn render_devices() -> AudioResult<Vec<IMMDevice>> {
	init_com();
	// SAFETY: COM calls with valid arguments; returned interfaces are reference counted.
	unsafe {
		let enumerator: IMMDeviceEnumerator =
			CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
				.map_err(|e| AudioError::Unavailable(e.to_string()))?;
		let collection = enumerator
			.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
			.map_err(win_error)?;
		let count = collection.GetCount().map_err(win_error)?;
		Ok((0..count).filter_map(|i| collection.Item(i).ok()).collect())
	}
}

fn sessions() -> AudioResult<Vec<Session>> {
	let mut result = Vec::new();
	for device in render_devices()? {
		let Ok(device_id) = device_id(&device) else {
			continue;
		};
		// SAFETY: COM calls on valid interface pointers.
		unsafe {
			let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
				continue;
			};
			let Ok(list) = manager.GetSessionEnumerator() else {
				continue;
			};
			let count = list.GetCount().unwrap_or(0);
			for i in 0..count {
				let Ok(control) = list.GetSession(i) else {
					continue;
				};
				let Ok(control) = control.cast::<IAudioSessionControl2>() else {
					continue;
				};
				let system = control.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK;
				if control
					.GetState()
					.is_ok_and(|s| s == AudioSessionStateExpired)
				{
					continue;
				}
				let pid = control.GetProcessId().unwrap_or(0);
				if pid == 0 && !system {
					continue;
				}
				let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
					continue;
				};
				let display_name = control
					.GetDisplayName()
					.ok()
					.and_then(take_co_string)
					// Resource references such as "@%SystemRoot%\..." are not readable names.
					.filter(|n| !n.is_empty() && !n.starts_with('@'));
				result.push(Session {
					pid,
					device_id: device_id.clone(),
					exe: if system { None } else { process_path(pid) },
					display_name,
					volume,
					system,
				});
			}
		}
	}
	Ok(result)
}

fn device_id(device: &IMMDevice) -> AudioResult<String> {
	// SAFETY: COM call on a valid interface pointer; the string is freed by take_co_string.
	let id = unsafe { device.GetId() }.map_err(win_error)?;
	take_co_string(id).ok_or(AudioError::NotFound)
}

fn device_name(device: &IMMDevice) -> Option<String> {
	// SAFETY: COM calls on valid interface pointers; the string is freed by take_co_string.
	unsafe {
		let store = device.OpenPropertyStore(STGM_READ).ok()?;
		let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
		take_co_string(PropVariantToStringAlloc(&value).ok()?)
	}
}

/// Convert a COM-allocated string and free it.
fn take_co_string(p: PWSTR) -> Option<String> {
	if p.is_null() {
		return None;
	}
	// SAFETY: `p` is a valid NUL-terminated string allocated with CoTaskMemAlloc.
	unsafe {
		let s = p.to_string().ok();
		CoTaskMemFree(Some(p.0 as *const c_void));
		s
	}
}

fn process_path(pid: u32) -> Option<PathBuf> {
	// SAFETY: the handle is checked and closed; the buffer outlives the call.
	unsafe {
		let handle: HANDLE = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
		let mut buffer = vec![0u16; 32_768];
		let mut len = buffer.len() as u32;
		let ok = QueryFullProcessImageNameW(
			handle,
			PROCESS_NAME_WIN32,
			PWSTR(buffer.as_mut_ptr()),
			&mut len,
		)
		.is_ok();
		let _ = CloseHandle(handle);
		ok.then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize])))
	}
}

fn app_key(exe: Option<&Path>, pid: u32) -> String {
	exe.and_then(|p| p.file_stem())
		.map(|n| n.to_string_lossy().to_lowercase())
		.unwrap_or_else(|| format!("pid-{pid}"))
}

/// `FileDescription` from the executable's version resource (e.g. "Google Chrome").
fn file_description(exe: &Path) -> Option<String> {
	let path = HSTRING::from(exe.as_os_str());
	// SAFETY: buffers are sized by the API and outlive every pointer derived from them.
	unsafe {
		let size = GetFileVersionInfoSizeW(&path, None);
		if size == 0 {
			return None;
		}
		let mut data = vec![0u8; size as usize];
		GetFileVersionInfoW(&path, None, size, data.as_mut_ptr().cast()).ok()?;

		// First language / code page pair of the resource.
		let mut ptr: *mut c_void = std::ptr::null_mut();
		let mut len = 0u32;
		let ok = VerQueryValueW(
			data.as_ptr().cast(),
			&HSTRING::from("\\VarFileInfo\\Translation"),
			&mut ptr,
			&mut len,
		);
		if !ok.as_bool() || len < 4 {
			return None;
		}
		let lang = *(ptr as *const u16);
		let codepage = *(ptr as *const u16).add(1);

		let query = HSTRING::from(format!(
			"\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription"
		));
		let ok = VerQueryValueW(data.as_ptr().cast(), &query, &mut ptr, &mut len);
		if !ok.as_bool() || len == 0 {
			return None;
		}
		let text = PCWSTR(ptr as *const u16).to_string().ok()?;
		let text = text.trim().to_owned();
		(!text.is_empty()).then_some(text)
	}
}

/// The policy API wants the device interface path, not the MMDevice id.
fn policy_device_id(mmdevice_id: &str) -> String {
	const RENDER_INTERFACE: &str = "{e6327cad-dcec-4949-ae8a-991e976a79d2}";
	format!("\\\\?\\SWD#MMDEVAPI#{mmdevice_id}#{RENDER_INTERFACE}")
}

/// `IAudioPolicyConfigFactory`: undocumented WinRT interface, as used by EarTrumpet.
/// Its vtable is IInspectable (6 slots), 19 methods this plugin does not use, then
/// `SetPersistedDefaultAudioEndpoint`.
const POLICY_IIDS: [GUID; 2] = [
	// Windows 10 21H2 and later.
	GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324),
	// Older Windows 10 builds.
	GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258),
];
const SET_PERSISTED_DEFAULT_ENDPOINT_SLOT: usize = 6 + 19;

type SetPersistedDefaultEndpointFn = unsafe extern "system" fn(
	this: *mut c_void,
	process_id: u32,
	flow: EDataFlow,
	role: ERole,
	device_id: *mut c_void,
) -> HRESULT;

struct PolicyConfig {
	/// Holds the reference returned by QueryInterface for one of [`POLICY_IIDS`].
	factory: IUnknown,
}

impl PolicyConfig {
	fn new() -> AudioResult<Self> {
		init_com();
		let class = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
		// SAFETY: activation of a system WinRT class; QueryInterface hands back an owned reference.
		unsafe {
			let inspectable: IInspectable = RoGetActivationFactory(&class)
				.map_err(|e| AudioError::Other(format!("audio policy API unavailable: {e}")))?;
			for iid in &POLICY_IIDS {
				let mut raw: *mut c_void = std::ptr::null_mut();
				if inspectable.query(iid, &mut raw).is_ok() && !raw.is_null() {
					return Ok(Self {
						factory: IUnknown::from_raw(raw),
					});
				}
			}
		}
		Err(AudioError::Other(
			"audio policy API: unknown interface version".into(),
		))
	}

	fn set_persisted_default_endpoint(
		&self,
		pid: u32,
		flow: EDataFlow,
		role: ERole,
		device: &HSTRING,
	) -> AudioResult<()> {
		let this = self.factory.as_raw();
		// SAFETY: `this` implements one of POLICY_IIDS, whose vtable has the method at this slot.
		// The HSTRING is passed by value as its raw handle and stays owned by `device`
		// (HSTRING is a transparent wrapper around that handle).
		let hr = unsafe {
			let vtable = *(this as *const *const usize);
			let method: SetPersistedDefaultEndpointFn =
				std::mem::transmute(*vtable.add(SET_PERSISTED_DEFAULT_ENDPOINT_SLOT));
			let handle: *mut c_void = std::mem::transmute_copy(device);
			method(this, pid, flow, role, handle)
		};
		hr.ok().map_err(win_error)
	}
}
