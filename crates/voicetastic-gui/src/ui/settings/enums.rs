//! `EnumStrings` impls for the prost enums referenced by the settings UI.
//!
//! Each entry lists every non-`UNRECOGNIZED` variant so the dropdown can
//! offer all firmware values the proto knows about. Add new variants here
//! when the upstream `meshtastic.proto` snapshot grows.

use voicetastic_core::proto::{
    channel,
    config::{
        bluetooth_config, device_config, display_config, lo_ra_config, network_config,
        position_config,
    },
};

use crate::impl_enum_strings;

impl_enum_strings!(
    lo_ra_config::RegionCode,
    [
        Unset, Us, Eu433, Eu868, Cn, Jp, Anz, Kr, Tw, Ru, In, Nz865, Th, Lora24, Ua433, Ua868,
        My433, My919, Sg923, Ph433, Ph868, Ph915, Anz433, Kz433, Kz863, Np865, Br902,
    ]
);
impl_enum_strings!(
    lo_ra_config::ModemPreset,
    [
        LongFast,
        LongSlow,
        VeryLongSlow,
        MediumSlow,
        MediumFast,
        ShortSlow,
        ShortFast,
        LongModerate,
        ShortTurbo,
    ]
);
impl_enum_strings!(
    device_config::Role,
    [
        Client,
        ClientMute,
        Router,
        RouterClient,
        Repeater,
        Tracker,
        Sensor,
        Tak,
        ClientHidden,
        LostAndFound,
        TakTracker,
        RouterLate,
    ]
);
impl_enum_strings!(
    device_config::RebroadcastMode,
    [
        All,
        AllSkipDecoding,
        LocalOnly,
        KnownOnly,
        None,
        CorePortnumsOnly,
    ]
);
impl_enum_strings!(position_config::GpsMode, [Disabled, Enabled, NotPresent]);
impl_enum_strings!(network_config::AddressMode, [Dhcp, Static]);
impl_enum_strings!(display_config::DisplayUnits, [Metric, Imperial]);
impl_enum_strings!(
    display_config::OledType,
    [
        OledAuto,
        OledSsd1306,
        OledSh1106,
        OledSh1107,
        OledSh1107128128
    ]
);
impl_enum_strings!(
    display_config::DisplayMode,
    [Default, Twocolor, Inverted, Color]
);
impl_enum_strings!(bluetooth_config::PairingMode, [RandomPin, FixedPin, NoPin]);
impl_enum_strings!(channel::Role, [Disabled, Primary, Secondary]);
