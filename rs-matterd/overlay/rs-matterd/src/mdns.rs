use rs_matter::dm::ChangeNotify;
use rs_matter::Matter;
use rs_matter::{crypto::Crypto, error::Error};
use socket2::{Domain, Protocol, Socket, Type};

pub async fn run_mdns<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    notify: &dyn ChangeNotify,
) -> Result<(), Error> {
    #[cfg(feature = "astro-dnssd")]
    rs_matter::transport::network::mdns::astro::AstroMdnsResponder::new(matter)
        .run(crypto, notify)
        .await?;

    #[cfg(all(feature = "zeroconf", not(feature = "astro-dnssd")))]
    rs_matter::transport::network::mdns::zeroconf::ZeroconfMdnsResponder::new(matter)
        .run(crypto, notify)
        .await?;

    #[cfg(all(
        feature = "resolve",
        not(any(feature = "zeroconf", feature = "astro-dnssd"))
    ))]
    rs_matter::transport::network::mdns::resolve::ResolveMdnsResponder::new(matter)
        .run(
            &rs_matter::utils::zbus::Connection::system().await.unwrap(),
            crypto,
            notify,
        )
        .await?;

    #[cfg(all(
        feature = "avahi",
        not(any(feature = "resolve", feature = "zeroconf", feature = "astro-dnssd"))
    ))]
    rs_matter::transport::network::mdns::avahi::AvahiMdnsResponder::new(matter)
        .run(
            &rs_matter::utils::zbus::Connection::system().await.unwrap(),
            crypto,
            notify,
        )
        .await?;

    #[cfg(not(any(
        feature = "avahi",
        feature = "resolve",
        feature = "zeroconf",
        feature = "astro-dnssd"
    )))]
    run_builtin_mdns(matter, crypto, notify).await?;

    Ok(())
}

async fn run_builtin_mdns<C: Crypto>(
    matter: &Matter<'_>,
    crypto: C,
    notify: &dyn ChangeNotify,
) -> Result<(), Error> {
    use std::net::UdpSocket;

    use log::info;
    use rs_matter::transport::network::{Ipv4Addr, Ipv6Addr};

    #[inline(never)]
    fn initialize_network() -> Result<(Ipv4Addr, Ipv6Addr, u32), Error> {
        use log::error;
        use nix::{net::if_::InterfaceFlags, sys::socket::SockaddrIn6};
        use rs_matter::error::ErrorCode;

        let interfaces = || {
            nix::ifaddrs::getifaddrs().unwrap().filter(|ia| {
                ia.flags
                    .contains(InterfaceFlags::IFF_UP | InterfaceFlags::IFF_BROADCAST)
                    && !ia
                        .flags
                        .intersects(InterfaceFlags::IFF_LOOPBACK | InterfaceFlags::IFF_POINTOPOINT)
            })
        };

        let (iname, ip, ipv6) = interfaces()
            .filter_map(|ia| {
                ia.address
                    .and_then(|addr| addr.as_sockaddr_in6().map(SockaddrIn6::ip))
                    .map(|ipv6| (ia.interface_name, ipv6))
            })
            .filter_map(|(iname, ipv6)| {
                interfaces()
                    .filter(|ia2| ia2.interface_name == iname)
                    .find_map(|ia2| {
                        ia2.address
                            .and_then(|addr| addr.as_sockaddr_in().map(|addr| addr.ip().into()))
                            .map(|ip: std::net::Ipv4Addr| (iname.clone(), ip, ipv6))
                    })
            })
            .next()
            .ok_or_else(|| {
                error!("cannot find network interface suitable for mDNS broadcasting");
                ErrorCode::StdIoError
            })?;

        info!("using interface {iname} with {ip}/{ipv6} for mDNS");

        Ok((ip.octets().into(), ipv6.octets().into(), 0))
    }

    let (ipv4_addr, ipv6_addr, interface) = initialize_network()?;

    use rs_matter::transport::network::mdns::builtin::{BuiltinMdnsResponder, Host};
    use rs_matter::transport::network::mdns::{
        MDNS_IPV4_BROADCAST_ADDR, MDNS_IPV6_BROADCAST_ADDR, MDNS_SOCKET_DEFAULT_BIND_ADDR,
    };

    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_only_v6(false)?;
    socket.bind(&MDNS_SOCKET_DEFAULT_BIND_ADDR.into())?;
    let socket = async_io::Async::<UdpSocket>::new_nonblocking(socket.into())?;

    socket
        .get_ref()
        .join_multicast_v6(&MDNS_IPV6_BROADCAST_ADDR, interface)?;
    socket
        .get_ref()
        .join_multicast_v4(&MDNS_IPV4_BROADCAST_ADDR, &ipv4_addr)?;

    BuiltinMdnsResponder::new(matter, crypto, notify)
        .run(
            &socket,
            &socket,
            &Host {
                id: 0,
                hostname: "homenode-rs-matterd",
                ip: ipv4_addr,
                ipv6: ipv6_addr,
            },
            Some(ipv4_addr),
            Some(interface),
        )
        .await
}
