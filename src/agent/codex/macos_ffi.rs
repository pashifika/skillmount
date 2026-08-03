//! Audited Core Foundation boundary for the Codex managed-preference probe.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::ptr;

type CfAllocatorRef = *const c_void;
type CfPropertyListRef = *const c_void;
type CfStringRef = *const c_void;

const UTF8_ENCODING: u32 = 0x0800_0100;
const MANAGED_DOMAIN: &[u8] = b"com.openai.codex";
const MANAGED_CONFIG_KEY: &[u8] = b"config_toml_base64";

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithBytes(
        allocator: CfAllocatorRef,
        bytes: *const u8,
        byte_count: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> CfStringRef;
    fn CFPreferencesCopyAppValue(
        key: CfStringRef,
        application_id: CfStringRef,
    ) -> CfPropertyListRef;
    fn CFPreferencesAppSynchronize(application_id: CfStringRef) -> u8;
    fn CFRelease(value: *const c_void);
}

/// Reports whether the Codex MDM configuration preference has any effective value.
pub(super) fn managed_configuration_present() -> io::Result<bool> {
    let domain = create_string(MANAGED_DOMAIN)?;

    // SAFETY: `domain` is a non-null live CFString object naming the exact application domain.
    // Synchronize performs no ownership transfer. It refreshes this process's cached preferences
    // so repeated probes observe external managed-layer changes before the following Copy call.
    let synchronized = unsafe { CFPreferencesAppSynchronize(domain) };
    if synchronized == 0 {
        release(domain);
        return Err(io::Error::other(
            "Core Foundation could not synchronize the managed-preference domain",
        ));
    }

    let key = match create_string(MANAGED_CONFIG_KEY) {
        Ok(key) => key,
        Err(error) => {
            release(domain);
            return Err(error);
        }
    };

    // SAFETY: `key` and `domain` are non-null live CFString objects created above. The Copy rule
    // returns either null or a retained property-list object, which is released below without
    // interpreting or exposing its raw representation.
    let value = unsafe { CFPreferencesCopyAppValue(key, domain) };
    release(domain);
    release(key);
    if !value.is_null() {
        release(value);
    }
    Ok(!value.is_null())
}

fn create_string(bytes: &[u8]) -> io::Result<CfStringRef> {
    let byte_count = isize::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "CFString input is too long"))?;
    // SAFETY: `bytes` remains alive for the call, its length is exact, null selects the default
    // allocator, and both fixed inputs are valid UTF-8. The Create rule transfers one retain to
    // this module, which every successful caller balances with `release`.
    let value = unsafe {
        CFStringCreateWithBytes(ptr::null(), bytes.as_ptr(), byte_count, UTF8_ENCODING, 0)
    };
    if value.is_null() {
        Err(io::Error::other(
            "Core Foundation could not allocate the managed-preference key",
        ))
    } else {
        Ok(value)
    }
}

fn release(value: *const c_void) {
    // SAFETY: every caller passes a non-null object returned under a Core Foundation Create or
    // Copy rule, exactly once. No raw object escapes this module.
    unsafe { CFRelease(value) };
}
