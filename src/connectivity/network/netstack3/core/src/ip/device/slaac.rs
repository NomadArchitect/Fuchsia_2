// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! IPv6 Stateless Address Autoconfiguration (SLAAC) as defined by [RFC 4862]
//! and temporary address extensions for SLAAC as defined by [RFC 8981].
//!
//! [RFC 4862]: https://datatracker.ietf.org/doc/html/rfc4862
//! [RFC 8981]: https://datatracker.ietf.org/doc/html/rfc8981

use alloc::{boxed::Box, vec::Vec};
use core::{
    convert::TryFrom,
    num::{NonZeroU64, NonZeroU8},
    time::Duration,
};

use log::{debug, error, trace};
use net_types::{
    ip::{AddrSubnet, IpAddress, Ipv6, Ipv6Addr, Subnet},
    UnicastAddr, Witness as _,
};
use num::rational::Ratio;
use packet_formats::{icmp::ndp::NonZeroNdpLifetime, utils::NonZeroDuration};
use rand::{distributions::Uniform, Rng as _, RngCore};

use crate::{
    algorithm::{
        generate_opaque_interface_identifier, OpaqueIidNonce, STABLE_IID_SECRET_KEY_BYTES,
    },
    context::{CounterContext, InstantContext, RngContext, TimerContext, TimerHandler},
    error::ExistsError,
    ip::{
        device::state::{DelIpv6AddrReason, Lifetime, SlaacConfig, TemporarySlaacConfig},
        IpDeviceIdContext,
    },
    Instant,
};

/// Minimum Valid Lifetime value to actually update an address's valid lifetime.
///
/// 2 hours.
const MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE: Duration = Duration::from_secs(7200);

/// Required prefix length for SLAAC.
///
/// We need 64 bits in the prefix because the interface identifier is 64 bits,
/// and IPv6 addresses are 128 bits.
const REQUIRED_PREFIX_BITS: u8 = 64;

// Host constants.

/// Maximum allowed DESYNC_FACTOR as a ratio of the TEMP_PREFERRED_LIFETIME as
/// described in [RFC 8981 Section 3.8].
///
/// [RFC 8981 Section 3.8]: http://tools.ietf.org/html/rfc8981#section-3.8
const MAX_DESYNC_FACTOR_RATIO: Ratio<u64> = Ratio::new_raw(2, 5);

#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
enum InnerSlaacTimerId {
    /// Timer to deprecate an address configured via SLAAC.
    DeprecateSlaacAddress { addr: UnicastAddr<Ipv6Addr> },
    /// Timer to invalidate an address configured via SLAAC.
    InvalidateSlaacAddress { addr: UnicastAddr<Ipv6Addr> },
    /// Timer to generate a new temporary SLAAC address before an existing one
    /// expires.
    RegenerateTemporaryAddress { addr_subnet: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>> },
}

/// A timer ID for SLAAC.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
pub(crate) struct SlaacTimerId<DeviceId> {
    device_id: DeviceId,
    inner: InnerSlaacTimerId,
}

impl<DeviceId> SlaacTimerId<DeviceId> {
    pub(crate) fn new_deprecate_slaac_address(
        device_id: DeviceId,
        addr: UnicastAddr<Ipv6Addr>,
    ) -> SlaacTimerId<DeviceId> {
        SlaacTimerId { device_id, inner: InnerSlaacTimerId::DeprecateSlaacAddress { addr } }
    }

    pub(crate) fn new_invalidate_slaac_address(
        device_id: DeviceId,
        addr: UnicastAddr<Ipv6Addr>,
    ) -> SlaacTimerId<DeviceId> {
        SlaacTimerId { device_id, inner: InnerSlaacTimerId::InvalidateSlaacAddress { addr } }
    }

    pub(crate) fn new_regenerate_temporary_slaac_address(
        device_id: DeviceId,
        addr_subnet: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
    ) -> SlaacTimerId<DeviceId> {
        SlaacTimerId {
            device_id,
            inner: InnerSlaacTimerId::RegenerateTemporaryAddress { addr_subnet },
        }
    }
}

/// The state associated with a SLAAC address.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct SlaacAddressEntry<Instant> {
    pub(super) addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
    pub(super) config: SlaacConfig<Instant>,
    pub(super) deprecated: bool,
}

/// A mutable view into state associated with a SLAAC address's mutable state.
pub(super) struct SlaacAddressEntryMut<'a, Instant> {
    pub(super) addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
    pub(super) config: &'a mut SlaacConfig<Instant>,
    pub(super) deprecated: &'a mut bool,
}

/// The state context provided to SLAAC.
pub(super) trait SlaacStateContext: IpDeviceIdContext<Ipv6> + InstantContext {
    /// Gets the configuration for SLAAC.
    fn get_config(&self, device_id: Self::DeviceId) -> SlaacConfiguration;

    /// Returns the number of DAD messages transmitted while performing DAD.
    ///
    /// `None` indicates that DAD is disabled.
    fn dad_transmits(&self, device_id: Self::DeviceId) -> Option<NonZeroU8>;

    /// Returns the configured NDP retransmission interval for the device.
    fn retrans_timer(&self, device_id: Self::DeviceId) -> Duration;

    /// Get the interface identifier for a device as defined by RFC 4291 section 2.5.1.
    fn get_interface_identifier(&self, device_id: Self::DeviceId) -> [u8; 8];

    /// Returns an iterator over the SLAAC addresses on the device.
    fn iter_slaac_addrs(
        &self,
        device_id: Self::DeviceId,
    ) -> Box<dyn Iterator<Item = SlaacAddressEntry<Self::Instant>> + '_>;

    /// Returns an iterator providing a mutable view of mutable SLAAC address
    /// state.
    fn iter_slaac_addrs_mut(
        &mut self,
        device_id: Self::DeviceId,
    ) -> Box<dyn Iterator<Item = SlaacAddressEntryMut<'_, Self::Instant>> + '_>;

    /// Adds a new IPv6 Global Address configured via SLAAC.
    fn add_slaac_addr_sub(
        &mut self,
        device_id: Self::DeviceId,
        addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
        slaac_config: SlaacConfig<Self::Instant>,
    ) -> Result<(), ExistsError>;

    /// Removes a SLAAC address.
    ///
    /// # Panics
    ///
    /// May panic if `addr` is not an address configured via SLAAC on
    /// `device_id`.
    fn remove_slaac_addr(&mut self, device_id: Self::DeviceId, addr: &UnicastAddr<Ipv6Addr>);
}

/// Update the instant at which an address configured via SLAAC is no longer
/// valid.
///
/// A `None` value for `valid_until` indicates that the address is valid
/// forever; `Some` indicates valid for some finite lifetime.
///
/// # Panics
///
/// May panic if `addr` is not an address configured via SLAAC on
/// `device_id`.
fn update_slaac_addr_valid_until<C: SlaacStateContext>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr: &UnicastAddr<Ipv6Addr>,
    valid_until: Lifetime<C::Instant>,
) {
    let slaac_config = sync_ctx
        .iter_slaac_addrs_mut(device_id)
        .find_map(|SlaacAddressEntryMut { addr_sub, config, deprecated: _ }| {
            (addr_sub.addr() == *addr).then(|| config)
        })
        .expect("address is not configured via SLAAC on this device");
    match slaac_config {
        SlaacConfig::Static { valid_until: v } => *v = valid_until,
        SlaacConfig::Temporary(TemporarySlaacConfig {
            valid_until: v,
            desync_factor: _,
            creation_time: _,
            dad_counter: _,
        }) => {
            *v = match valid_until {
                Lifetime::Finite(v) => v,
                Lifetime::Infinite => panic!("temporary addresses may not be valid forever"),
            }
        }
    };
}

/// The execution context for SLAAC.
trait SlaacContext:
    SlaacStateContext + TimerContext<SlaacTimerId<Self::DeviceId>> + CounterContext + RngContext
{
}

impl<
        C: SlaacStateContext
            + TimerContext<SlaacTimerId<Self::DeviceId>>
            + CounterContext
            + RngContext,
    > SlaacContext for C
{
}

/// An implementation of SLAAC.
pub(crate) trait SlaacHandler: IpDeviceIdContext<Ipv6> + InstantContext {
    /// Executes the algorithm in [RFC 4862 Section 5.5.3], with the extensions
    /// from [RFC 8981 Section 3.4] for temporary addresses, for a given prefix
    /// advertised by a router.
    ///
    /// This function updates all static and temporary SLAAC addresses for the
    /// given prefix and adds new ones if necessary.
    ///
    /// [RFC 4862 Section 5.5.3]: http://tools.ietf.org/html/rfc4862#section-5.5.3
    /// [RFC 8981 Section 3.4]: https://tools.ietf.org/html/rfc8981#section-3.4
    fn apply_slaac_update(
        &mut self,
        device_id: Self::DeviceId,
        prefix: Subnet<Ipv6Addr>,
        preferred_lifetime: Option<NonZeroNdpLifetime>,
        valid_lifetime: Option<NonZeroNdpLifetime>,
    );

    /// Handles SLAAC specific aspects of address removal.
    ///
    /// Must only be called after the address is removed from the interface.
    fn on_address_removed(
        &mut self,
        device_id: Self::DeviceId,
        addr: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
        state: SlaacConfig<Self::Instant>,
        reason: DelIpv6AddrReason,
    );
}

impl<C: SlaacContext> SlaacHandler for C {
    fn apply_slaac_update(
        &mut self,
        device_id: Self::DeviceId,
        subnet: Subnet<Ipv6Addr>,
        preferred_lifetime: Option<NonZeroNdpLifetime>,
        valid_lifetime: Option<NonZeroNdpLifetime>,
    ) {
        if preferred_lifetime > valid_lifetime {
            // If the preferred lifetime is greater than the valid lifetime,
            // silently ignore the Prefix Information option, as per RFC 4862
            // section 5.5.3.
            trace!("receive_ndp_packet: autonomous prefix's preferred lifetime is greater than valid lifetime, ignoring");
            return;
        }

        let now = self.now();
        let existing_subnet_slaac_addrs: Vec<_> =
            self.iter_slaac_addrs(device_id).filter(|a| a.addr_sub.subnet() == subnet).collect();

        // Apply the update to each existing address, static or temporary, for the
        // prefix.
        for entry in existing_subnet_slaac_addrs.iter() {
            let addr_sub = entry.addr_sub;
            let addr = addr_sub.addr();
            let slaac_config = &entry.config;

            trace!(
            "receive_ndp_packet: already have a {:?} SLAAC address {:?} configured on device {:?}",
            SlaacType::from(slaac_config),
            addr_sub,
            device_id
        );

            /// Encapsulates a lifetime bound and where it came from.
            #[derive(Copy, Clone)]
            enum ValidLifetimeBound {
                FromPrefix(Option<NonZeroNdpLifetime>),
                FromMaxBound(Duration),
            }

            impl ValidLifetimeBound {
                /// Unwraps the object and returns the wrapped duration.
                fn get(self) -> Option<NonZeroNdpLifetime> {
                    match self {
                        Self::FromPrefix(d) => d,
                        Self::FromMaxBound(d) => {
                            NonZeroDuration::new(d).map(NonZeroNdpLifetime::Finite)
                        }
                    }
                }
            }

            let (valid_for, entry_valid_until, preferred_for_and_regen_at) = match slaac_config {
                SlaacConfig::Static { valid_until: entry_valid_until } => (
                    ValidLifetimeBound::FromPrefix(valid_lifetime),
                    *entry_valid_until,
                    preferred_lifetime.map(|p| (p, None)),
                ),
                // Select valid_for and preferred_for according to RFC 8981
                // Section 3.4.
                SlaacConfig::Temporary(TemporarySlaacConfig {
                    valid_until: entry_valid_until,
                    creation_time,
                    desync_factor,
                    dad_counter: _,
                }) => {
                    let SlaacConfiguration { temporary_address_configuration } =
                        self.get_config(device_id);
                    let (valid_for, preferred_for, entry_valid_until) =
                        match temporary_address_configuration {
                            // Since it's possible to change NDP configuration for a
                            // device during runtime, we can end up here, with a
                            // temporary address on an interface even though temporary
                            // addressing is disabled. Setting its validity period to 0
                            // will force it to be removed ASAP.
                            None => (
                                ValidLifetimeBound::FromMaxBound(Duration::ZERO),
                                None,
                                *entry_valid_until,
                            ),
                            Some(temporary_address_config) => {
                                // RFC 8981 Section 3.4.2:
                                //   When updating the preferred lifetime of an existing
                                //   temporary address, it would be set to expire at
                                //   whichever time is earlier: the time indicated by
                                //   the received lifetime or (CREATION_TIME +
                                //   TEMP_PREFERRED_LIFETIME - DESYNC_FACTOR). A similar
                                //   approach can be used with the valid lifetime.
                                let preferred_for =
                                    preferred_lifetime.and_then(|preferred_lifetime| {
                                        temporary_address_config
                                            .temp_preferred_lifetime
                                            .get()
                                            .checked_sub(now.duration_since(*creation_time))
                                            .and_then(|p| p.checked_sub(*desync_factor))
                                            .and_then(NonZeroDuration::new)
                                            .map(|d| preferred_lifetime.min_finite_duration(d))
                                    });
                                // Per RFC 8981 Section 3.4.1, `desync_factor` is only
                                // used for preferred lifetime:
                                //   [...] with the overall constraint that no temporary
                                //   addresses should ever remain "valid" or "preferred"
                                //   for a time longer than (TEMP_VALID_LIFETIME) or
                                //   (TEMP_PREFERRED_LIFETIME - DESYNC_FACTOR),
                                //   respectively.
                                let since_creation = now.duration_since(*creation_time);
                                let configured_max_lifetime =
                                    temporary_address_config.temp_valid_lifetime.get();
                                let max_valid_lifetime = if since_creation > configured_max_lifetime
                                {
                                    Duration::ZERO
                                } else {
                                    configured_max_lifetime - since_creation
                                };

                                let valid_for = valid_lifetime.map_or(
                                    ValidLifetimeBound::FromPrefix(None),
                                    |d| match d {
                                        NonZeroNdpLifetime::Infinite => {
                                            ValidLifetimeBound::FromMaxBound(max_valid_lifetime)
                                        }
                                        NonZeroNdpLifetime::Finite(d) => {
                                            if max_valid_lifetime <= d.get() {
                                                ValidLifetimeBound::FromMaxBound(max_valid_lifetime)
                                            } else {
                                                ValidLifetimeBound::FromPrefix(valid_lifetime)
                                            }
                                        }
                                    },
                                );

                                (valid_for, preferred_for, *entry_valid_until)
                            }
                        };

                    let preferred_for_and_regen_at = preferred_for.map(|preferred_for| {
                        let dad_transmits = self.dad_transmits(device_id);
                        let SlaacConfiguration { temporary_address_configuration } =
                            self.get_config(device_id);

                        let regen_at = temporary_address_configuration.and_then(
                            |TemporarySlaacAddressConfiguration {
                                 temp_idgen_retries,
                                 temp_preferred_lifetime: _,
                                 temp_valid_lifetime: _,
                                 secret_key: _,
                             }| {
                                let regen_advance = regen_advance(
                                    temp_idgen_retries,
                                    self.retrans_timer(device_id),
                                    dad_transmits.map_or(0, NonZeroU8::get),
                                )
                                .get();
                                // Per RFC 8981 Section 3.6:
                                //
                                //   Hosts following this specification SHOULD
                                //   generate new temporary addresses over time.
                                //   This can be achieved by generating a new
                                //   temporary address REGEN_ADVANCE time units
                                //   before a temporary address becomes deprecated.
                                //
                                // It's possible for regen_at to be before the
                                // current time. In that case, set it to `now` so
                                // that a new address is generated after the current
                                // prefix information is handled.
                                preferred_for
                                    .get()
                                    .checked_sub(regen_advance)
                                    .map_or(Some(now), |d| now.checked_add(d))
                            },
                        );

                        (NonZeroNdpLifetime::Finite(preferred_for), regen_at)
                    });

                    (valid_for, Lifetime::Finite(entry_valid_until), preferred_for_and_regen_at)
                }
            };

            // `Some` iff the remaining lifetime is a positive non-zero lifetime.
            let remaining_lifetime = match entry_valid_until {
                Lifetime::Infinite => Some(Lifetime::Infinite),
                Lifetime::Finite(entry_valid_until) => (entry_valid_until > now)
                    .then(|| Lifetime::Finite(entry_valid_until.duration_since(now))),
            };

            // As per RFC 4862 section 5.5.3.e, if the advertised prefix is equal to
            // the prefix of an address configured by stateless autoconfiguration in
            // the list, the preferred lifetime of the address is reset to the
            // Preferred Lifetime in the received advertisement.

            // Update the preferred lifetime for this address.
            //
            // Must not have reached this point if the address was not already
            // assigned to a device.
            let entry = self
                .iter_slaac_addrs_mut(device_id)
                .find(|a| a.addr_sub == entry.addr_sub)
                .unwrap();
            match preferred_for_and_regen_at {
                None => {
                    if !*entry.deprecated {
                        *entry.deprecated = true;
                        let _: Option<C::Instant> = self.cancel_timer(
                            SlaacTimerId::new_deprecate_slaac_address(device_id, addr),
                        );
                        let _: Option<C::Instant> = self.cancel_timer(
                            SlaacTimerId::new_regenerate_temporary_slaac_address(
                                device_id, addr_sub,
                            ),
                        );
                    }
                }
                Some((preferred_for, regen_at)) => {
                    if *entry.deprecated {
                        *entry.deprecated = false;
                    }

                    let timer_id =
                        SlaacTimerId::new_deprecate_slaac_address(device_id, addr).into();
                    let _previously_scheduled_instant: Option<C::Instant> = match preferred_for {
                        NonZeroNdpLifetime::Finite(preferred_for) => {
                            // Use `schedule_timer_instant` instead of `schedule_timer` to set
                            // the timeout relative to the previously recorded `now` value. This
                            // helps prevent skew in cases where this task gets preempted and
                            // isn't scheduled for some period of time between recording `now`
                            // and here.
                            self.schedule_timer_instant(
                                now.checked_add(preferred_for.get()).unwrap(),
                                timer_id,
                            )
                        }
                        NonZeroNdpLifetime::Infinite => self.cancel_timer(timer_id),
                    };

                    let _prev_regen_at: Option<C::Instant> = match regen_at {
                        Some(regen_at) => self.schedule_timer_instant(
                            regen_at,
                            SlaacTimerId::new_regenerate_temporary_slaac_address(
                                device_id, addr_sub,
                            ),
                        ),
                        None => {
                            self.cancel_timer(SlaacTimerId::new_regenerate_temporary_slaac_address(
                                device_id, addr_sub,
                            ))
                        }
                    };
                }
            }

            // As per RFC 4862 section 5.5.3.e, the specific action to perform for
            // the valid lifetime of the address depends on the Valid Lifetime in
            // the received advertisement and the remaining time to the valid
            // lifetime expiration of the previously autoconfigured address:
            let valid_for_to_update = match valid_for {
                ValidLifetimeBound::FromMaxBound(valid_for) => {
                    // If the maximum lifetime for the address is smaller than the
                    // lifetime specified for the prefix, then it must be applied.
                    NonZeroDuration::new(valid_for).map(NonZeroNdpLifetime::Finite)
                }
                ValidLifetimeBound::FromPrefix(valid_for) => {
                    // If the received Valid Lifetime is greater than 2 hours or
                    // greater than RemainingLifetime, set the valid lifetime of
                    // the corresponding address to the advertised Valid
                    // Lifetime.
                    match valid_for {
                        Some(NonZeroNdpLifetime::Infinite) => Some(NonZeroNdpLifetime::Infinite),
                        Some(NonZeroNdpLifetime::Finite(v))
                            if v.get() > MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE
                                || remaining_lifetime
                                    .map_or(true, |r| r < Lifetime::Finite(v.get())) =>
                        {
                            Some(NonZeroNdpLifetime::Finite(v))
                        }
                        None | Some(NonZeroNdpLifetime::Finite(_)) => {
                            if remaining_lifetime.map_or(true, |r| {
                                r <= Lifetime::Finite(MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE)
                            }) {
                                // If RemainingLifetime is less than or equal to 2 hours,
                                // ignore the Prefix Information option with regards to the
                                // valid lifetime, unless the Router Advertisement from
                                // which this option was obtained has been authenticated
                                // (e.g., via Secure Neighbor Discovery [RFC3971]).  If the
                                // Router Advertisement was authenticated, the valid
                                // lifetime of the corresponding address should be set to
                                // the Valid Lifetime in the received option.
                                //
                                // TODO(ghanan): If the NDP packet this prefix option is in
                                //               was authenticated, update the valid
                                //               lifetime of the address to the valid
                                //               lifetime in the received option, as per RFC
                                //               4862 section 5.5.3.e.
                                None
                            } else {
                                // Otherwise, reset the valid lifetime of the corresponding
                                // address to 2 hours.
                                Some(NonZeroNdpLifetime::Finite(
                                    NonZeroDuration::new(MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE)
                                        .unwrap(),
                                ))
                            }
                        }
                    }
                }
            };

            match valid_for_to_update {
                Some(valid_for) => match valid_for {
                    NonZeroNdpLifetime::Finite(valid_for) => {
                        let valid_until = now.checked_add(valid_for.get()).unwrap();
                        trace!("receive_ndp_packet: updating valid lifetime to {:?} for SLAAC address {:?} on device {:?}", valid_until, addr, device_id);

                        // Set the valid lifetime for this address.
                        update_slaac_addr_valid_until(
                            self,
                            device_id,
                            &addr,
                            Lifetime::Finite(valid_until),
                        );

                        let _: Option<C::Instant> = self.schedule_timer_instant(
                            valid_until,
                            SlaacTimerId::new_invalidate_slaac_address(device_id, addr).into(),
                        );
                    }
                    NonZeroNdpLifetime::Infinite => {
                        // Set the valid lifetime for this address.
                        update_slaac_addr_valid_until(self, device_id, &addr, Lifetime::Infinite);

                        let _: Option<C::Instant> = self.cancel_timer(
                            SlaacTimerId::new_invalidate_slaac_address(device_id, addr).into(),
                        );
                    }
                },
                None => {
                    trace!("receive_ndp_packet: not updating valid lifetime for SLAAC address {:?} on device {:?} as remaining lifetime is less than 2 hours and new valid lifetime ({:?}) is less than remaining lifetime", addr, device_id, valid_for.get());
                }
            }
        }

        // As per RFC 4862 section 5.5.3.e, if the prefix advertised is not equal to
        // the prefix of an address configured by stateless autoconfiguration
        // already in the list of addresses associated with the interface, and if
        // the Valid Lifetime is not 0, form an address (and add it to the list) by
        // combining the advertised prefix with an interface identifier of the link
        // as follows:
        //
        // |    128 - N bits    |        N bits          |
        // +--------------------+------------------------+
        // |    link prefix     |  interface identifier  |
        // +---------------------------------------------+
        let valid_lifetime = match valid_lifetime {
            Some(valid_lifetime) => valid_lifetime,
            None => {
                trace!("receive_ndp_packet: autonomous prefix has valid lifetime = 0, ignoring");
                return;
            }
        };
        let address_types_to_add =
            IntoIterator::into_iter([SlaacType::Static, SlaacType::Temporary]).filter(
                |slaac_type| {
                    let mut of_same_slaac_type = existing_subnet_slaac_addrs.iter().filter(|a| {
                        match a.config {
                            SlaacConfig::Static { valid_until: _ } => {
                                // From RFC 4862 Section 5.5.3.d: "If the prefix advertised
                                // is not equal to the prefix of an address configured by
                                // stateless autoconfiguration already in the list of
                                // addresses associated with the interface (where 'equal'
                                // means the two prefix lengths are the same and the first
                                // prefix- length bits of the prefixes are identical), and
                                // if the Valid Lifetime is not 0, form an address [...]"
                                *slaac_type == SlaacType::Static
                            }
                            SlaacConfig::Temporary(_) => {
                                // From RFC 8981 Section 3.4.3: "If the host has not
                                // configured any temporary address for the corresponding
                                // prefix, the host SHOULD create a new temporary address
                                // for such prefix."
                                *slaac_type == SlaacType::Temporary
                            }
                        }
                    });
                    // Add addresses only if there are none already present.
                    of_same_slaac_type.next() == None
                },
            );

        for slaac_type in address_types_to_add {
            add_slaac_addr_sub(
                self,
                device_id,
                now,
                SlaacInitConfig::new(slaac_type),
                valid_lifetime,
                preferred_lifetime,
                &subnet,
            );
        }
    }

    fn on_address_removed(
        &mut self,
        device_id: Self::DeviceId,
        addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
        state: SlaacConfig<C::Instant>,
        reason: DelIpv6AddrReason,
    ) {
        let preferred_until = self
            .cancel_timer(SlaacTimerId::new_deprecate_slaac_address(device_id, addr_sub.addr()));
        let _valid_until: Option<C::Instant> = self
            .cancel_timer(SlaacTimerId::new_invalidate_slaac_address(device_id, addr_sub.addr()));

        let TemporarySlaacConfig { valid_until, creation_time, desync_factor, dad_counter } =
            match state {
                SlaacConfig::Temporary(temporary_config) => {
                    let _regen_at: Option<C::Instant> = self.cancel_timer(
                        SlaacTimerId::new_regenerate_temporary_slaac_address(device_id, addr_sub),
                    );
                    temporary_config
                }
                SlaacConfig::Static { .. } => return,
            };

        match reason {
            DelIpv6AddrReason::ManualAction => return,
            DelIpv6AddrReason::DadFailed => {
                // Attempt to regenerate the address.
            }
        }

        let SlaacConfiguration { temporary_address_configuration } = self.get_config(device_id);
        let temporary_address_configuration = match temporary_address_configuration {
            Some(configuration) => configuration,
            None => return,
        };

        if dad_counter >= temporary_address_configuration.temp_idgen_retries {
            return;
        }

        let temp_valid_lifetime = temporary_address_configuration.temp_valid_lifetime;
        // Compute the original preferred lifetime for the removed address so that
        // it can be used for the new address being generated. If, when the address
        // was created, the prefix's preferred lifetime was less than
        // `temporary_address_configuration.temp_preferred_lifetime`, then that's
        // what will be calculated here. That's fine because it's a lower bound on
        // the prefix's value, which means the prefix's value is still being
        // respected.
        let preferred_for = match preferred_until
            .map(|preferred_until| preferred_until.duration_since(creation_time) + desync_factor)
        {
            Some(preferred_for) => preferred_for,
            // If the address is already deprecated, a new address should already
            // have been generated, so ignore this one.
            None => return,
        };

        let now = self.now();
        // It's possible this `valid_for` value is larger than `temp_valid_lifetime`
        // (e.g. if the NDP configuration was changed since this address was
        // generated). That's okay, because `add_slaac_addr_sub` will apply the
        // current maximum valid lifetime when called below.
        let valid_for = NonZeroDuration::new(valid_until.duration_since(creation_time))
            .unwrap_or(temp_valid_lifetime);

        add_slaac_addr_sub(
            self,
            device_id,
            now,
            SlaacInitConfig::Temporary { dad_count: dad_counter + 1 },
            NonZeroNdpLifetime::Finite(valid_for),
            NonZeroDuration::new(preferred_for).map(NonZeroNdpLifetime::Finite),
            &addr_sub.subnet(),
        );
    }
}

impl<C: SlaacContext> TimerHandler<SlaacTimerId<C::DeviceId>> for C {
    fn handle_timer(&mut self, SlaacTimerId { device_id, inner }: SlaacTimerId<C::DeviceId>) {
        match inner {
            InnerSlaacTimerId::DeprecateSlaacAddress { addr } => {
                set_deprecated_slaac_addr(self, device_id, &addr, true)
            }
            InnerSlaacTimerId::InvalidateSlaacAddress { addr } => {
                self.remove_slaac_addr(device_id, &addr);
            }
            InnerSlaacTimerId::RegenerateTemporaryAddress { addr_subnet } => {
                regenerate_temporary_slaac_addr(self, device_id, &addr_subnet);
            }
        }
    }
}

/// Configuration values for SLAAC temporary addressing.
///
/// The algorithm specified in [RFC 8981 Section 3.4] references several
/// configuration parameters, which are defined in [Section 3.8] and
/// [Section 3.3.2] This struct contains the following values specified by the
/// RFC:
/// - TEMP_VALID_LIFETIME
/// - TEMP_PREFERRED_LIFETIME
/// - TEMP_IDGEN_RETRIES
/// - secret_key
///
/// [RFC 8981 Section 3.4]: http://tools.ietf.org/html/rfc8981#section-3.4
/// [Section 3.3.2]: http://tools.ietf.org/html/rfc8981#section-3.3.2
/// [Section 3.8]: http://tools.ietf.org/html/rfc8981#section-3.8
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TemporarySlaacAddressConfiguration {
    /// The maximum amount of time that a temporary address can be considered
    /// valid, from the time of its creation.
    pub temp_valid_lifetime: NonZeroDuration,

    /// The maximum amount of time that a temporary address can be preferred,
    /// from the time of its creation.
    pub temp_preferred_lifetime: NonZeroDuration,

    /// The number of times to attempt to pick a new temporary address after DAD
    /// detects a duplicate before stopping and giving up on temporary address
    /// generation for that prefix.
    pub temp_idgen_retries: u8,

    /// The secret to use when generating new temporary addresses. This should
    /// be initialized from a random number generator before generating any
    /// temporary addresses.
    pub secret_key: [u8; STABLE_IID_SECRET_KEY_BYTES],
}

/// The configuration for SLAAC.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct SlaacConfiguration {
    /// Configuration for temporary address assignment.
    ///
    /// If `None`, temporary addresses will not be assigned to interfaces, and
    /// any already-assigned temporary addresses will be removed.
    ///
    /// If Some, specifies the configuration parameters for temporary addressing,
    /// including those relating to how long temporary addresses should remain
    /// preferred and valid.
    pub temporary_address_configuration: Option<TemporarySlaacAddressConfiguration>,
}

#[derive(PartialEq, Eq)]
enum SlaacType {
    Static,
    Temporary,
}

impl core::fmt::Debug for SlaacType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SlaacType::Static => f.write_str("static"),
            SlaacType::Temporary => f.write_str("temporary"),
        }
    }
}

impl<'a, Instant> From<&'a SlaacConfig<Instant>> for SlaacType {
    fn from(slaac_config: &'a SlaacConfig<Instant>) -> Self {
        match slaac_config {
            SlaacConfig::Static { .. } => SlaacType::Static,
            SlaacConfig::Temporary { .. } => SlaacType::Temporary,
        }
    }
}

fn set_deprecated_slaac_addr<C: SlaacContext>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr: &UnicastAddr<Ipv6Addr>,
    deprecated: bool,
) {
    let entry =
        sync_ctx.iter_slaac_addrs_mut(device_id).find(|a| &a.addr_sub.addr() == addr).unwrap();
    *entry.deprecated = deprecated;
}

/// Computes REGEN_ADVANCE as specified in [RFC 8981 Section 3.8].
///
/// [RFC 8981 Section 3.8]: http://tools.ietf.org/html/rfc8981#section-3.8
fn regen_advance(
    temp_idgen_retries: u8,
    retrans_timer: Duration,
    dad_transmits: u8,
) -> NonZeroDuration {
    const TWO_SECONDS: NonZeroDuration =
        NonZeroDuration::from_nonzero_secs(const_unwrap::const_unwrap_option(NonZeroU64::new(2)));
    // Per the RFC, REGEN_ADVANCE in seconds =
    //   2 + (TEMP_IDGEN_RETRIES * DupAddrDetectTransmits * RetransTimer / 1000)
    //
    // where RetransTimer is in milliseconds. Since values here are kept as
    // Durations, there is no need to apply scale factors.
    TWO_SECONDS
        + retrans_timer
            .checked_mul(u32::from(temp_idgen_retries) * u32::from(dad_transmits))
            .unwrap_or(Duration::ZERO)
}

fn regenerate_temporary_slaac_addr<C: SlaacContext>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    addr_subnet: &AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
) {
    let entry = {
        let mut subnet_addrs = sync_ctx
            .iter_slaac_addrs(device_id)
            .filter(|entry| entry.addr_sub.subnet() == addr_subnet.subnet() && !entry.deprecated);

        // It's possible that there are multiple non-deprecated temporary
        // addresses in a subnet for this host (if prefix updates are received
        // after regen but before deprecation). Per RFC 8981 Section 3.5:
        //
        //   Note that, in normal operation, except for the transient period
        //   when a temporary address is being regenerated, at most one
        //   temporary address per prefix should be in a nondeprecated state at
        //   any given time on a given interface.
        //
        // In order to tend towards only one non-deprecated temporary address on
        // a subnet, we ignore all but the last regen timer for the
        // non-deprecated addresses in a subnet.
        if let Some((entry, regen_at)) = subnet_addrs.find_map(|entry| {
            sync_ctx
                .scheduled_instant(SlaacTimerId::new_regenerate_temporary_slaac_address(
                    device_id,
                    entry.addr_sub,
                ))
                .map(|instant| (entry, instant))
        }) {
            debug!(
                "regenerate_temporary_addr: ignoring regen event at {:?} for {:?} since {:?} will regenerate after at {:?}",
                sync_ctx.now(), addr_subnet, entry.addr_sub.addr(), regen_at);
            return;
        }
        match sync_ctx.iter_slaac_addrs(device_id).find(|entry| &entry.addr_sub == addr_subnet) {
            Some(entry) => entry,
            None => unreachable!("couldn't find {:?} to regenerate", addr_subnet),
        }
    };
    assert!(!entry.deprecated, "can't regenerate deprecated address {:?}", addr_subnet);

    let TemporarySlaacConfig { creation_time, desync_factor, valid_until, dad_counter: _ } =
        match entry.config {
            SlaacConfig::Temporary(temporary_config) => temporary_config,
            SlaacConfig::Static { valid_until: _ } => unreachable!(
                "can't regenerate a temporary address for {:?}, which is static",
                addr_subnet
            ),
        };

    let SlaacConfiguration { temporary_address_configuration } = sync_ctx.get_config(device_id);
    let TemporarySlaacAddressConfiguration {
        temp_valid_lifetime,
        temp_preferred_lifetime: _,
        temp_idgen_retries: _,
        secret_key: _,
    } = match temporary_address_configuration {
        Some(configuration) => configuration,
        None => return,
    };

    let deprecate_at = sync_ctx
        .scheduled_instant(SlaacTimerId::new_deprecate_slaac_address(device_id, addr_subnet.addr()))
        .unwrap_or_else(|| unreachable!(
            "temporary SLAAC address {:?} had a regen timer fire but does not have a deprecation timer",
            addr_subnet.addr()
        ));
    let preferred_for = deprecate_at.duration_since(creation_time) + desync_factor;

    let now = sync_ctx.now();
    // It's possible this `valid_for` value is larger than `temp_valid_lifetime`
    // (e.g. if the NDP configuration was changed since this address was
    // generated). That's okay, because `add_slaac_addr_sub` will apply the
    // current maximum valid lifetime when called below.
    let valid_for = NonZeroDuration::new(valid_until.duration_since(creation_time))
        .unwrap_or(temp_valid_lifetime);

    add_slaac_addr_sub(
        sync_ctx,
        device_id,
        now,
        SlaacInitConfig::Temporary { dad_count: 0 },
        NonZeroNdpLifetime::Finite(valid_for),
        NonZeroDuration::new(preferred_for).map(NonZeroNdpLifetime::Finite),
        &addr_subnet.subnet(),
    );
}

#[derive(Copy, Clone, Debug)]
enum SlaacInitConfig {
    Static,
    Temporary { dad_count: u8 },
}

impl SlaacInitConfig {
    fn new(slaac_type: SlaacType) -> Self {
        match slaac_type {
            SlaacType::Static => Self::Static,
            SlaacType::Temporary => Self::Temporary { dad_count: 0 },
        }
    }
}

/// Checks whether the address has an IID that doesn't conflict with existing
/// IANA reserved ranges.
///
/// Compares against the ranges defined by various RFCs and listed at
/// https://www.iana.org/assignments/ipv6-interface-ids/ipv6-interface-ids.xhtml
fn has_iana_allowed_iid(address: Ipv6Addr) -> bool {
    let mut iid = [0u8; 8];
    const U64_SUFFIX_LEN: usize = Ipv6Addr::BYTES as usize - u64::BITS as usize / 8;
    iid.copy_from_slice(&address.bytes()[U64_SUFFIX_LEN..]);
    let iid = u64::from_be_bytes(iid);
    match iid {
        // Subnet-Router Anycast
        0x0000_0000_0000_0000 => false,
        // Consolidated match for
        // - Ethernet Block: 0x200:5EFF:FE00:0000-0200:4EFF:FE00:5212
        // - Proxy Mobile: 0x200:5EFF:FE00:5213
        // - Ethernet Block: 0x200:5EFF:FE00:5214-0200:4EFF:FEFF:FFFF
        0x0200_5EFF_FE00_0000..=0x0200_5EFF_FEFF_FFFF => false,
        // Subnet Anycast Addresses
        0xFDFF_FFFF_FFFF_FF80..=0xFDFF_FFFF_FFFF_FFFF => false,

        // All other IIDs not in the reserved ranges
        _iid => true,
    }
}

/// Generate an IPv6 Global Address as defined by RFC 4862 section 5.5.3.d.
///
/// The generated address will be of the format:
///
/// |            128 - N bits               |       N bits           |
/// +---------------------------------------+------------------------+
/// |            link prefix                |  interface identifier  |
/// +----------------------------------------------------------------+
///
/// # Panics
///
/// Panics if a valid IPv6 unicast address cannot be formed with the provided
/// prefix and interface identifier, or if the prefix length is not a multiple
/// of 8 bits.
fn generate_global_static_address(
    prefix: &Subnet<Ipv6Addr>,
    iid: &[u8],
) -> AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>> {
    if prefix.prefix() % 8 != 0 {
        unimplemented!("generate_global_address: not implemented for when prefix length is not a multiple of 8 bits");
    }

    let prefix_len = prefix.prefix() / u8::try_from(u8::BITS).unwrap();

    assert_eq!(usize::from(Ipv6Addr::BYTES - prefix_len), iid.len());

    let mut address = prefix.network().ipv6_bytes();
    address[prefix_len.into()..].copy_from_slice(&iid);

    let address = AddrSubnet::new(Ipv6Addr::from(address), prefix.prefix()).unwrap();
    assert_eq!(address.subnet(), *prefix);

    address
}

/// Generate a temporary IPv6 Global Address as defined by RFC 8981 section 3.4.6
///
/// The generated address will be of the format:
///
/// |            128 - N bits              |        N bits           |
/// +--------------------------------------+-------------------------+
/// |            link prefix               |  randomized identifier  |
/// +----------------------------------------------------------------+
///
/// # Panics
///
/// Panics if a valid IPv6 unicast address cannot be formed with the provided
/// prefix, or if the prefix length is not a multiple of 8 bits.
fn generate_global_temporary_address(
    prefix: &Subnet<Ipv6Addr>,
    iid: &[u8; 8],
    seed: u64,
    secret_key: &[u8; STABLE_IID_SECRET_KEY_BYTES],
) -> AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>> {
    let prefix_len = usize::from(prefix.prefix() / 8);

    assert_eq!(usize::from(Ipv6Addr::BYTES) - prefix_len, iid.len());
    let mut address = prefix.network().ipv6_bytes();

    let interface_identifier = generate_opaque_interface_identifier(
        /* prefix */ *prefix,
        /* net_iface */ iid,
        /* net_id */ [],
        /* nonce */ OpaqueIidNonce::Random(seed),
        /* secret_key */ secret_key,
    );
    let suffix_bytes = &interface_identifier.to_be_bytes()[..(address.len() - prefix_len)];
    address[prefix_len..].copy_from_slice(suffix_bytes);

    let address = AddrSubnet::new(Ipv6Addr::from(address), prefix.prefix()).unwrap();
    assert_eq!(address.subnet(), *prefix);

    address
}

fn add_slaac_addr_sub<C: SlaacContext>(
    sync_ctx: &mut C,
    device_id: C::DeviceId,
    now: C::Instant,
    slaac_config: SlaacInitConfig,
    prefix_valid_for: NonZeroNdpLifetime,
    prefix_preferred_for: Option<NonZeroNdpLifetime>,
    subnet: &Subnet<Ipv6Addr>,
) {
    if subnet.prefix() != REQUIRED_PREFIX_BITS {
        // If the sum of the prefix length and interface identifier length does
        // not equal 128 bits, the Prefix Information option MUST be ignored, as
        // per RFC 4862 section 5.5.3.
        error!("receive_ndp_packet: autonomous prefix length {:?} and interface identifier length {:?} cannot form valid IPv6 address, ignoring", subnet.prefix(), REQUIRED_PREFIX_BITS);
        return;
    }

    struct PreferredForAndRegenAt<Instant>(NonZeroNdpLifetime, Option<Instant>);

    let (valid_until, preferred_and_regen, slaac_config, mut addresses) = match slaac_config {
        SlaacInitConfig::Static => {
            let valid_until = match prefix_valid_for {
                NonZeroNdpLifetime::Finite(d) => {
                    Lifetime::Finite(now.checked_add(d.get()).unwrap())
                }
                NonZeroNdpLifetime::Infinite => Lifetime::Infinite,
            };
            (
                valid_until,
                prefix_preferred_for.map(|p| PreferredForAndRegenAt(p, None)),
                SlaacConfig::Static { valid_until },
                // Generate the global address as defined by RFC 4862 section 5.5.3.d.
                //
                // TODO(https://fxbug.dev/95946): Support regenerating address.
                either::Either::Left(core::iter::once(generate_global_static_address(
                    &subnet,
                    &sync_ctx.get_interface_identifier(device_id)[..],
                ))),
            )
        }
        SlaacInitConfig::Temporary { dad_count } => {
            let per_attempt_random_seed = sync_ctx.rng_mut().next_u64();
            let dad_transmits = sync_ctx.dad_transmits(device_id);
            let SlaacConfiguration { temporary_address_configuration } =
                sync_ctx.get_config(device_id);
            let temporary_address_config = match temporary_address_configuration {
                Some(temporary_address_config) => temporary_address_config,
                None => {
                    trace!(
                        "receive_ndp_packet: temporary addresses are disabled on device {:?}",
                        device_id
                    );
                    return;
                }
            };

            // Per RFC 8981 Section 3.4.4:
            //    When creating a temporary address, DESYNC_FACTOR MUST be computed
            //    and associated with the newly created address, and the address
            //    lifetime values MUST be derived from the corresponding prefix as
            //    follows:
            //
            //    *  Its valid lifetime is the lower of the Valid Lifetime of the
            //       prefix and TEMP_VALID_LIFETIME.
            //
            //    *  Its preferred lifetime is the lower of the Preferred Lifetime
            //       of the prefix and TEMP_PREFERRED_LIFETIME - DESYNC_FACTOR.
            let valid_for = match prefix_valid_for {
                NonZeroNdpLifetime::Finite(prefix_valid_for) => {
                    core::cmp::min(prefix_valid_for, temporary_address_config.temp_valid_lifetime)
                }
                NonZeroNdpLifetime::Infinite => temporary_address_config.temp_valid_lifetime,
            };

            let regen_advance = regen_advance(
                temporary_address_config.temp_idgen_retries,
                sync_ctx.retrans_timer(device_id),
                dad_transmits.map_or(0, NonZeroU8::get),
            )
            .get();

            let iid = sync_ctx.get_interface_identifier(device_id);
            let secret_key = temporary_address_config.secret_key;
            let mut seed = per_attempt_random_seed;
            let addresses = either::Either::Right(core::iter::repeat_with(move || {
                // RFC 8981 Section 3.3.3 specifies that
                //
                //   The resulting IID MUST be compared against the reserved
                //   IPv6 IIDs and against those IIDs already employed in an
                //   address of the same network interface and the same network
                //   prefix.  In the event that an unacceptable identifier has
                //   been generated, the DAD_Counter should be incremented by 1,
                //   and the algorithm should be restarted from the first step.
                loop {
                    let address =
                        generate_global_temporary_address(&subnet, &iid, seed, &secret_key);
                    seed = seed.wrapping_add(1);

                    if has_iana_allowed_iid(address.addr().get()) {
                        break address;
                    }
                }
            }));

            let valid_until = now.checked_add(valid_for.get()).unwrap();

            let temp_preferred_lifetime = temporary_address_config.temp_preferred_lifetime.get();
            // Per RFC 8981 Section 3.8:
            //    DESYNC_FACTOR
            //       A random value within the range 0 - MAX_DESYNC_FACTOR.  It
            //       is computed each time a temporary address is generated, and
            //       is associated with the corresponding address.  It MUST be
            //       smaller than (TEMP_PREFERRED_LIFETIME - REGEN_ADVANCE).
            let max_desync_factor = core::cmp::min(
                // Using second accuracy will only cause problems here if
                // temp_preferred_lifetime is < 1s, in which case
                // `preferred_for.checked_sub(regen_advance)` below will be None
                // and an address will not be generated.
                Duration::from_secs(
                    (MAX_DESYNC_FACTOR_RATIO * temp_preferred_lifetime.as_secs()).to_integer(),
                ),
                temp_preferred_lifetime - regen_advance,
            );

            let desync_factor =
                sync_ctx.rng_mut().sample(Uniform::new(Duration::ZERO, max_desync_factor));
            let preferred_for = prefix_preferred_for.and_then(|prefix_preferred_for| {
                temp_preferred_lifetime
                    .checked_sub(desync_factor)
                    .and_then(NonZeroDuration::new)
                    .map(|d| prefix_preferred_for.min_finite_duration(d))
            });

            // RFC 8981 Section 3.4.5:
            //
            //   A temporary address is created only if this calculated
            //   preferred lifetime is greater than REGEN_ADVANCE time
            //   units.
            let preferred_for_and_regen_at = match preferred_for {
                None => return,
                Some(preferred_for) => match preferred_for.get().checked_sub(regen_advance) {
                    Some(before_regen) => PreferredForAndRegenAt(
                        NonZeroNdpLifetime::Finite(preferred_for),
                        Some(now.checked_add(before_regen).unwrap()),
                    ),
                    None => {
                        trace!("receive_ndp_packet: preferred lifetime of {:?} for subnet {:?} is too short to allow regen", preferred_for, subnet);
                        return;
                    }
                },
            };
            (
                Lifetime::Finite(valid_until),
                Some(preferred_for_and_regen_at),
                SlaacConfig::Temporary(TemporarySlaacConfig {
                    desync_factor,
                    valid_until,
                    creation_time: now,
                    dad_counter: dad_count,
                }),
                addresses,
            )
        }
    };

    // Attempt to add the address to the device.
    let address = loop {
        let address = match addresses.next() {
            Some(address) => address,
            // No more addresses to try - do nothing further.
            None => {
                trace!(
                    "exhausted possible SLAAC addresses without assigning on device {:?}",
                    device_id
                );
                return;
            }
        };

        // TODO(https://fxbug.dev/91301): Should bindings be the one to actually
        // assign the address to maintain a "single source of truth"?
        match sync_ctx.add_slaac_addr_sub(device_id, address, slaac_config) {
            Err(ExistsError) => {
                trace!("IPv6 SLAAC address {:?} already exists on device {:?}", address, device_id);

                // Try the next address.
                //
                // TODO(https://fxbug.dev/100003): Limit number of attempts.
                sync_ctx.increment_counter("generated_slaac_addr_exists");
            }
            Ok(()) => break address,
        }
    };

    trace!("receive_ndp_packet: Successfully configured new IPv6 address {:?} on device {:?} via SLAAC", address, device_id);

    // Set the valid lifetime for this address.
    //
    // Must not have reached this point if the address was already assigned
    // to a device.
    match valid_until {
        Lifetime::Finite(valid_until) => {
            assert_eq!(
                sync_ctx.schedule_timer_instant(
                    valid_until,
                    SlaacTimerId::new_invalidate_slaac_address(device_id, address.addr()).into(),
                ),
                None
            );
        }
        Lifetime::Infinite => {}
    }

    let deprecate_timer_id = SlaacTimerId::new_deprecate_slaac_address(device_id, address.addr());

    // Set the preferred lifetime for this address.
    //
    // Must not have reached this point if the address was already assigned
    // to a device.
    match preferred_and_regen {
        // Use `schedule_timer_instant` instead of `schedule_timer` to set the timeout
        // relative to the previously recorded `now` value. This helps prevent skew in
        // cases where this task gets preempted and isn't scheduled for some period of time
        // between recording `now` and here.
        Some(PreferredForAndRegenAt(preferred_for, regen_at)) => {
            match preferred_for {
                NonZeroNdpLifetime::Finite(preferred_for) => {
                    assert_eq!(
                        sync_ctx.schedule_timer_instant(
                            now.checked_add(preferred_for.get()).unwrap(),
                            deprecate_timer_id.into()
                        ),
                        None
                    );
                }
                NonZeroNdpLifetime::Infinite => {}
            }

            match regen_at {
                Some(regen_at) => assert_eq!(
                    sync_ctx.schedule_timer_instant(
                        regen_at,
                        SlaacTimerId::new_regenerate_temporary_slaac_address(device_id, address)
                            .into()
                    ),
                    None
                ),
                None => (),
            }
        }
        None => {
            set_deprecated_slaac_addr(sync_ctx, device_id, &address.addr(), true);
            assert_eq!(sync_ctx.cancel_timer(deprecate_timer_id.into()), None);
        }
    };
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use fakealloc::collections::hash_map::{Entry, HashMap};
    use net_declare::net::ip_v6;
    use test_case::test_case;

    use super::*;
    use crate::{
        context::testutil::{DummyCtx, DummyInstant, DummyTimerCtxExt as _},
        ip::DummyDeviceId,
        testutil::assert_empty,
    };

    struct MockSlaacContext {
        config: SlaacConfiguration,
        dad_transmits: Option<NonZeroU8>,
        retrans_timer: Duration,
        iid: [u8; 8],
        slaac_addrs: HashMap<UnicastAddr<Ipv6Addr>, SlaacAddressEntry<DummyInstant>>,
        non_slaac_addr: Option<UnicastAddr<Ipv6Addr>>,
    }

    type MockCtx = DummyCtx<MockSlaacContext, SlaacTimerId<DummyDeviceId>, (), (), DummyDeviceId>;

    impl SlaacStateContext for MockCtx {
        fn get_config(&self, DummyDeviceId: Self::DeviceId) -> SlaacConfiguration {
            let MockSlaacContext {
                config,
                dad_transmits: _,
                retrans_timer: _,
                iid: _,
                slaac_addrs: _,
                non_slaac_addr: _,
            } = self.get_ref();
            *config
        }

        fn dad_transmits(&self, DummyDeviceId: Self::DeviceId) -> Option<NonZeroU8> {
            let MockSlaacContext {
                config: _,
                dad_transmits,
                retrans_timer: _,
                iid: _,
                slaac_addrs: _,
                non_slaac_addr: _,
            } = self.get_ref();
            *dad_transmits
        }

        fn retrans_timer(&self, DummyDeviceId: Self::DeviceId) -> Duration {
            let MockSlaacContext {
                config: _,
                dad_transmits: _,
                retrans_timer,
                iid: _,
                slaac_addrs: _,
                non_slaac_addr: _,
            } = self.get_ref();
            *retrans_timer
        }

        fn get_interface_identifier(&self, DummyDeviceId: Self::DeviceId) -> [u8; 8] {
            let MockSlaacContext {
                config: _,
                dad_transmits: _,
                retrans_timer: _,
                iid,
                slaac_addrs: _,
                non_slaac_addr: _,
            } = self.get_ref();
            *iid
        }

        fn iter_slaac_addrs(
            &self,
            DummyDeviceId: Self::DeviceId,
        ) -> Box<dyn Iterator<Item = SlaacAddressEntry<Self::Instant>> + '_> {
            let MockSlaacContext {
                config: _,
                dad_transmits: _,
                retrans_timer: _,
                iid: _,
                slaac_addrs,
                non_slaac_addr: _,
            } = self.get_ref();
            Box::new(slaac_addrs.values().cloned())
        }

        fn iter_slaac_addrs_mut(
            &mut self,
            DummyDeviceId: Self::DeviceId,
        ) -> Box<dyn Iterator<Item = SlaacAddressEntryMut<'_, Self::Instant>> + '_> {
            let MockSlaacContext {
                config: _,
                dad_transmits: _,
                retrans_timer: _,
                iid: _,
                slaac_addrs,
                non_slaac_addr: _,
            } = self.get_mut();
            Box::new(slaac_addrs.values_mut().map(
                |SlaacAddressEntry { addr_sub, config, deprecated }| SlaacAddressEntryMut {
                    addr_sub: *addr_sub,
                    config,
                    deprecated,
                },
            ))
        }

        fn add_slaac_addr_sub(
            &mut self,
            DummyDeviceId: Self::DeviceId,
            addr_sub: AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>>,
            config: SlaacConfig<Self::Instant>,
        ) -> Result<(), ExistsError> {
            let MockSlaacContext {
                config: _,
                dad_transmits: _,
                retrans_timer: _,
                iid: _,
                slaac_addrs,
                non_slaac_addr,
            } = self.get_mut();

            if non_slaac_addr.map_or(false, |a| a == addr_sub.addr()) {
                return Err(ExistsError);
            }

            match slaac_addrs.entry(addr_sub.addr()) {
                Entry::Occupied(_) => Err(ExistsError),
                Entry::Vacant(e) => {
                    let _inserted_val: &mut SlaacAddressEntry<_> =
                        e.insert(SlaacAddressEntry { addr_sub, config, deprecated: false });
                    Ok(())
                }
            }
        }

        fn remove_slaac_addr(
            &mut self,
            DummyDeviceId: Self::DeviceId,
            addr: &UnicastAddr<Ipv6Addr>,
        ) {
            let MockSlaacContext {
                config: _,
                dad_transmits: _,
                retrans_timer: _,
                iid: _,
                slaac_addrs,
                non_slaac_addr: _,
            } = self.get_mut();
            let _removed_val: Option<SlaacAddressEntry<_>> = slaac_addrs.remove(addr);
        }
    }

    #[test_case(ip_v6!("1:2:3:4::"), false; "subnet-router anycast")]
    #[test_case(ip_v6!("::1"), true; "allowed 1")]
    #[test_case(ip_v6!("1:2:3:4::1"), true; "allowed 2")]
    #[test_case(ip_v6!("4:4:4:4:0200:5eff:fe00:1"), false; "first ethernet block")]
    #[test_case(ip_v6!("1:1:1:1:0200:5eff:fe00:5213"), false; "proxy mobile")]
    #[test_case(ip_v6!("8:8:8:8:0200:5eff:fe00:8000"), false; "second ethernet block")]
    #[test_case(ip_v6!("a:a:a:a:fdff:ffff:ffff:ffaa"), false; "subnet anycast")]
    #[test_case(ip_v6!("c:c:c:c:fe00::"), true; "allowed 3")]
    fn test_has_iana_allowed_iid(addr: Ipv6Addr, expect_allowed: bool) {
        assert_eq!(has_iana_allowed_iid(addr), expect_allowed);
    }

    const IID: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
    const DEFAULT_RETRANS_TIMER: Duration = Duration::from_secs(1);
    const SUBNET: Subnet<Ipv6Addr> =
        unsafe { Subnet::new_unchecked(Ipv6Addr::new([0x200a, 0, 0, 0, 0, 0, 0, 0]), 64) };

    #[test_case(0, 0; "zero lifetimes")]
    #[test_case(2, 1; "preferred larger than valid")]
    fn dont_generate_address(preferred_lifetime_secs: u32, valid_lifetime_secs: u32) {
        let mut ctx = MockCtx::with_state(MockSlaacContext {
            config: SlaacConfiguration::default(),
            dad_transmits: None,
            retrans_timer: DEFAULT_RETRANS_TIMER,
            iid: IID,
            slaac_addrs: Default::default(),
            non_slaac_addr: None,
        });

        SlaacHandler::apply_slaac_update(
            &mut ctx,
            DummyDeviceId,
            SUBNET,
            NonZeroNdpLifetime::from_u32_with_infinite(preferred_lifetime_secs),
            NonZeroNdpLifetime::from_u32_with_infinite(valid_lifetime_secs),
        );
        assert_empty(ctx.iter_slaac_addrs(DummyDeviceId));
        ctx.timer_ctx().assert_no_timers_installed();
    }

    fn calculate_addr_sub(
        subnet: Subnet<Ipv6Addr>,
        iid: [u8; 8],
    ) -> AddrSubnet<Ipv6Addr, UnicastAddr<Ipv6Addr>> {
        let mut bytes = subnet.network().ipv6_bytes();
        bytes[8..].copy_from_slice(&iid);
        AddrSubnet::new(Ipv6Addr::from_bytes(bytes), subnet.prefix()).unwrap()
    }

    #[test_case(0; "deprecated")]
    #[test_case(1; "preferred")]
    fn generate_stable_address(preferred_lifetime_secs: u32) {
        let mut ctx = MockCtx::with_state(MockSlaacContext {
            config: SlaacConfiguration::default(),
            dad_transmits: None,
            retrans_timer: DEFAULT_RETRANS_TIMER,
            iid: IID,
            slaac_addrs: Default::default(),
            non_slaac_addr: None,
        });

        let valid_lifetime_secs = preferred_lifetime_secs + 1;
        let addr_sub = calculate_addr_sub(SUBNET, IID);

        // Generate a new SLAAC address.
        SlaacHandler::apply_slaac_update(
            &mut ctx,
            DummyDeviceId,
            SUBNET,
            NonZeroNdpLifetime::from_u32_with_infinite(preferred_lifetime_secs),
            NonZeroNdpLifetime::from_u32_with_infinite(valid_lifetime_secs),
        );
        let address_created_deprecated = preferred_lifetime_secs == 0;
        let now = ctx.now();
        let valid_until = now + Duration::from_secs(valid_lifetime_secs.into());
        let entry = SlaacAddressEntry {
            addr_sub,
            config: SlaacConfig::Static { valid_until: Lifetime::Finite(valid_until) },
            deprecated: address_created_deprecated,
        };
        assert_eq!(ctx.iter_slaac_addrs(DummyDeviceId).collect::<Vec<_>>(), [entry],);
        let deprecate_timer_id =
            SlaacTimerId::new_deprecate_slaac_address(DummyDeviceId, addr_sub.addr());
        let invalidate_timer_id =
            SlaacTimerId::new_invalidate_slaac_address(DummyDeviceId, addr_sub.addr());
        if address_created_deprecated {
            ctx.timer_ctx().assert_timers_installed([(invalidate_timer_id, valid_until)]);
        } else {
            ctx.timer_ctx().assert_timers_installed([
                (deprecate_timer_id, now + Duration::from_secs(preferred_lifetime_secs.into())),
                (invalidate_timer_id, valid_until),
            ]);

            // Trigger the deprecation timer.
            assert_eq!(
                ctx.trigger_next_timer(TimerHandler::handle_timer),
                Some(deprecate_timer_id)
            );
            let entry = SlaacAddressEntry { deprecated: true, ..entry };
            assert_eq!(ctx.iter_slaac_addrs(DummyDeviceId).collect::<Vec<_>>(), [entry],);
            ctx.timer_ctx().assert_timers_installed([(invalidate_timer_id, valid_until)]);
        }

        // Trigger the invalidation timer.
        assert_eq!(ctx.trigger_next_timer(TimerHandler::handle_timer), Some(invalidate_timer_id));
        assert_empty(ctx.iter_slaac_addrs(DummyDeviceId));
        ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test]
    fn stable_address_conflict() {
        let addr_sub = calculate_addr_sub(SUBNET, IID);

        let mut ctx = MockCtx::with_state(MockSlaacContext {
            config: SlaacConfiguration::default(),
            dad_transmits: None,
            retrans_timer: DEFAULT_RETRANS_TIMER,
            iid: IID,
            slaac_addrs: Default::default(),
            // Consider the address we will generate as already assigned without
            // SLAAC.
            non_slaac_addr: Some(addr_sub.addr()),
        });

        const LIFETIME_SECS: u32 = 1;

        // Generate a new SLAAC address.
        SlaacHandler::apply_slaac_update(
            &mut ctx,
            DummyDeviceId,
            SUBNET,
            NonZeroNdpLifetime::from_u32_with_infinite(LIFETIME_SECS),
            NonZeroNdpLifetime::from_u32_with_infinite(LIFETIME_SECS),
        );
        assert_empty(ctx.iter_slaac_addrs(DummyDeviceId));
        ctx.timer_ctx().assert_no_timers_installed();
    }

    #[test_case(DelIpv6AddrReason::ManualAction; "manual action")]
    #[test_case(DelIpv6AddrReason::DadFailed; "dad failed")]
    fn remove_stable_address(reason: DelIpv6AddrReason) {
        let addr_sub = calculate_addr_sub(SUBNET, IID);

        let mut ctx = MockCtx::with_state(MockSlaacContext {
            config: SlaacConfiguration::default(),
            dad_transmits: None,
            retrans_timer: DEFAULT_RETRANS_TIMER,
            iid: IID,
            slaac_addrs: Default::default(),
            non_slaac_addr: None,
        });

        const LIFETIME_SECS: u32 = 1;

        // Generate a new SLAAC address.
        SlaacHandler::apply_slaac_update(
            &mut ctx,
            DummyDeviceId,
            SUBNET,
            NonZeroNdpLifetime::from_u32_with_infinite(LIFETIME_SECS),
            NonZeroNdpLifetime::from_u32_with_infinite(LIFETIME_SECS),
        );
        let now = ctx.now();
        let valid_until = now + Duration::from_secs(LIFETIME_SECS.into());
        let entry = SlaacAddressEntry {
            addr_sub,
            config: SlaacConfig::Static { valid_until: Lifetime::Finite(valid_until) },
            deprecated: false,
        };
        assert_eq!(ctx.iter_slaac_addrs(DummyDeviceId).collect::<Vec<_>>(), [entry],);
        let deprecate_timer_id =
            SlaacTimerId::new_deprecate_slaac_address(DummyDeviceId, addr_sub.addr());
        let invalidate_timer_id =
            SlaacTimerId::new_invalidate_slaac_address(DummyDeviceId, addr_sub.addr());
        ctx.timer_ctx().assert_timers_installed([
            (deprecate_timer_id, now + Duration::from_secs(LIFETIME_SECS.into())),
            (invalidate_timer_id, valid_until),
        ]);

        // Remove the address and let SLAAC know the address was removed
        let config = assert_matches!(
            ctx.get_mut().slaac_addrs.remove(&addr_sub.addr()),
            Some(SlaacAddressEntry { addr_sub: got_addr_sub, config, deprecated }) => {
                assert_eq!(addr_sub, got_addr_sub);
                assert!(!deprecated);
                config
            }
        );
        SlaacHandler::on_address_removed(&mut ctx, DummyDeviceId, addr_sub, config, reason);
        ctx.timer_ctx().assert_no_timers_installed();
    }

    struct RefreshStableAddressTimersTest {
        orig_pl_secs: u32,
        orig_vl_secs: u32,
        new_pl_secs: u32,
        new_vl_secs: u32,
        effective_new_vl_secs: u32,
    }

    const ONE_HOUR_AS_SECS: u32 = 60 * 60;
    const TWO_HOURS_AS_SECS: u32 = ONE_HOUR_AS_SECS * 2;
    const THREE_HOURS_AS_SECS: u32 = ONE_HOUR_AS_SECS * 3;
    const INFINITE_LIFETIME: u32 = u32::MAX;
    const MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS: u32 =
        MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE.as_secs() as u32;
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: 1,
        orig_vl_secs: 1,
        new_pl_secs: 1,
        new_vl_secs: 1,
        effective_new_vl_secs: 1,
    }; "do nothing")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: 1,
        orig_vl_secs: 1,
        new_pl_secs: 2,
        new_vl_secs: 2,
        effective_new_vl_secs: 2,
    }; "increase lifetimes")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: 1,
        orig_vl_secs: 1,
        new_pl_secs: 0,
        new_vl_secs: 1,
        effective_new_vl_secs: 1,
    }; "deprecate address only")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: 0,
        orig_vl_secs: 1,
        new_pl_secs: 1,
        new_vl_secs: 1,
        effective_new_vl_secs: 1,
    }; "undeprecate address")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: 1,
        orig_vl_secs: 1,
        new_pl_secs: 0,
        new_vl_secs: 0,
        effective_new_vl_secs: 1,
    }; "deprecate address only with new valid lifetime of zero")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: ONE_HOUR_AS_SECS,
        orig_vl_secs: ONE_HOUR_AS_SECS,
        new_pl_secs: ONE_HOUR_AS_SECS - 1,
        new_vl_secs: ONE_HOUR_AS_SECS - 1,
        effective_new_vl_secs: ONE_HOUR_AS_SECS,
    }; "decrease preferred lifetime and ignore new valid lifetime if less than 2 hours and remaining lifetime")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: THREE_HOURS_AS_SECS,
        orig_vl_secs: THREE_HOURS_AS_SECS,
        new_pl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS - 1,
        new_vl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS - 1,
        effective_new_vl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS,
    }; "deprecate address only and bring valid lifetime down to 2 hours at max")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: ONE_HOUR_AS_SECS - 1,
        orig_vl_secs: ONE_HOUR_AS_SECS - 1,
        new_pl_secs: ONE_HOUR_AS_SECS - 1,
        new_vl_secs: ONE_HOUR_AS_SECS,
        effective_new_vl_secs: ONE_HOUR_AS_SECS,
    }; "increase valid lifetime if more than remaining valid lifetime")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: INFINITE_LIFETIME,
        orig_vl_secs: INFINITE_LIFETIME,
        new_pl_secs: INFINITE_LIFETIME,
        new_vl_secs: INFINITE_LIFETIME,
        effective_new_vl_secs: INFINITE_LIFETIME,
    }; "infinite lifetimes")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: ONE_HOUR_AS_SECS,
        orig_vl_secs: TWO_HOURS_AS_SECS,
        new_pl_secs: TWO_HOURS_AS_SECS,
        new_vl_secs: INFINITE_LIFETIME,
        effective_new_vl_secs: INFINITE_LIFETIME,
    }; "update valid lifetime from finite to infinite")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: ONE_HOUR_AS_SECS,
        orig_vl_secs: TWO_HOURS_AS_SECS,
        new_pl_secs: INFINITE_LIFETIME,
        new_vl_secs: INFINITE_LIFETIME,
        effective_new_vl_secs: INFINITE_LIFETIME,
    }; "update both lifetimes from finite to infinite")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: TWO_HOURS_AS_SECS,
        orig_vl_secs: INFINITE_LIFETIME,
        new_pl_secs: ONE_HOUR_AS_SECS,
        new_vl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS - 1,
        effective_new_vl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS,
    }; "update valid lifetime from infinite to finite")]
    #[test_case(RefreshStableAddressTimersTest {
        orig_pl_secs: INFINITE_LIFETIME,
        orig_vl_secs: INFINITE_LIFETIME,
        new_pl_secs: ONE_HOUR_AS_SECS,
        new_vl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS - 1,
        effective_new_vl_secs: MIN_PREFIX_VALID_LIFETIME_FOR_UPDATE_AS_SECS,
    }; "update both lifetimes from infinite to finite")]
    fn stable_address_timers(
        RefreshStableAddressTimersTest {
            orig_pl_secs,
            orig_vl_secs,
            new_pl_secs,
            new_vl_secs,
            effective_new_vl_secs,
        }: RefreshStableAddressTimersTest,
    ) {
        let mut ctx = MockCtx::with_state(MockSlaacContext {
            config: SlaacConfiguration::default(),
            dad_transmits: None,
            retrans_timer: DEFAULT_RETRANS_TIMER,
            iid: IID,
            slaac_addrs: Default::default(),
            non_slaac_addr: None,
        });

        let addr_sub = calculate_addr_sub(SUBNET, IID);
        let deprecate_timer_id =
            SlaacTimerId::new_deprecate_slaac_address(DummyDeviceId, addr_sub.addr());
        let invalidate_timer_id =
            SlaacTimerId::new_invalidate_slaac_address(DummyDeviceId, addr_sub.addr());

        // Generate a new SLAAC address.
        let ndp_pl = NonZeroNdpLifetime::from_u32_with_infinite(orig_pl_secs);
        let ndp_vl = NonZeroNdpLifetime::from_u32_with_infinite(orig_vl_secs);
        SlaacHandler::apply_slaac_update(&mut ctx, DummyDeviceId, SUBNET, ndp_pl, ndp_vl);
        let address_created_deprecated = ndp_pl.is_none();
        let now = ctx.now();
        let mut expected_timers = Vec::new();
        let valid_until = match ndp_vl.expect("this test expects to create an address") {
            NonZeroNdpLifetime::Finite(d) => {
                let valid_until = now + d.get();
                expected_timers.push((invalidate_timer_id, valid_until));
                Lifetime::Finite(valid_until)
            }
            NonZeroNdpLifetime::Infinite => Lifetime::Infinite,
        };
        match ndp_pl {
            None | Some(NonZeroNdpLifetime::Infinite) => {}
            Some(NonZeroNdpLifetime::Finite(d)) => {
                expected_timers.push((deprecate_timer_id, now + d.get()))
            }
        }
        let entry = SlaacAddressEntry {
            addr_sub,
            config: SlaacConfig::Static { valid_until },
            deprecated: address_created_deprecated,
        };
        assert_eq!(ctx.iter_slaac_addrs(DummyDeviceId).collect::<Vec<_>>(), [entry],);
        ctx.timer_ctx().assert_timers_installed(expected_timers);

        // Refresh timers.
        let ndp_pl = NonZeroNdpLifetime::from_u32_with_infinite(new_pl_secs);
        SlaacHandler::apply_slaac_update(
            &mut ctx,
            DummyDeviceId,
            SUBNET,
            ndp_pl,
            NonZeroNdpLifetime::from_u32_with_infinite(new_vl_secs),
        );
        let mut expected_timers = Vec::new();
        let valid_until = match NonZeroNdpLifetime::from_u32_with_infinite(effective_new_vl_secs)
            .expect("this test expects to keep the address")
        {
            NonZeroNdpLifetime::Finite(d) => {
                let valid_until = now + d.get();
                expected_timers.push((invalidate_timer_id, valid_until));
                Lifetime::Finite(valid_until)
            }
            NonZeroNdpLifetime::Infinite => Lifetime::Infinite,
        };
        match ndp_pl {
            None | Some(NonZeroNdpLifetime::Infinite) => {}
            Some(NonZeroNdpLifetime::Finite(d)) => {
                expected_timers.push((deprecate_timer_id, now + d.get()))
            }
        }
        let entry = SlaacAddressEntry {
            config: SlaacConfig::Static { valid_until },
            deprecated: ndp_pl.is_none(),
            ..entry
        };
        assert_eq!(ctx.iter_slaac_addrs(DummyDeviceId).collect::<Vec<_>>(), [entry],);
        ctx.timer_ctx().assert_timers_installed(expected_timers);
    }
}
