use thiserror::Error;
use windows::Networking::Connectivity::{
    ConnectionProfile, NetworkConnectivityLevel, NetworkCostType, NetworkInformation,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemConditions {
    pub network_available: bool,
    pub on_battery: bool,
    pub metered_network: bool,
}

impl SystemConditions {
    pub fn query() -> Result<Self, ConditionsError> {
        let mut power = SYSTEM_POWER_STATUS::default();
        // SAFETY: power is a live writable SYSTEM_POWER_STATUS value.
        unsafe { GetSystemPowerStatus(&raw mut power) }?;

        let profiles = NetworkInformation::GetConnectionProfiles()?;
        let mut active_profiles = 0_u32;
        let mut every_active_profile_is_metered = true;
        for index in 0..profiles.Size()? {
            let profile = profiles.GetAt(index)?;
            if profile.GetNetworkConnectivityLevel()? == NetworkConnectivityLevel::None {
                continue;
            }
            active_profiles += 1;
            every_active_profile_is_metered &= profile_is_metered(&profile)?;
        }

        Ok(Self {
            network_available: active_profiles > 0,
            // Unknown AC state is treated conservatively as battery power.
            on_battery: power.ACLineStatus != 1,
            metered_network: active_profiles > 0 && every_active_profile_is_metered,
        })
    }

    pub const fn conservative() -> Self {
        Self {
            network_available: false,
            on_battery: true,
            metered_network: true,
        }
    }
}

fn profile_is_metered(profile: &ConnectionProfile) -> Result<bool, windows::core::Error> {
    let cost = profile.GetConnectionCost()?;
    Ok(cost.NetworkCostType()? != NetworkCostType::Unrestricted
        || cost.Roaming()?
        || cost.OverDataLimit()?)
}

/// Initializes the Windows Runtime on the service event-loop thread.
pub struct WinRtApartment;

impl WinRtApartment {
    pub fn initialize() -> Result<Self, windows::core::Error> {
        // SAFETY: the service event loop owns this thread and balances a
        // successful initialization with RoUninitialize on the same thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
        Ok(Self)
    }
}

impl Drop for WinRtApartment {
    fn drop(&mut self) {
        // SAFETY: this instance exists only after a successful RoInitialize on
        // this thread and is dropped before the service thread exits.
        unsafe { RoUninitialize() };
    }
}

#[derive(Debug, Error)]
pub enum ConditionsError {
    #[error("Windows could not report current power or network conditions: {0}")]
    Windows(#[from] windows::core::Error),
}
