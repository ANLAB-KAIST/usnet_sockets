# usnet_sockets: Socket library for Rust using smoltcp as userspace network stack

The goal of usnet_sockets is to provide a compatible drop-in for the Rust standard library types `TcpStream` and `TcpListener`. This means not only on the type level but also in terms of behavior and interaction with other programs on a Linux system.
It is part of the master thesis [“Memory-safe Network Services Through A Userspace Networking Switch”](https://pothos.github.io/papers/msc_thesis_memory-safe_network_services_userspace_switch.pdf), for a short version see the defense [presentation](https://pothos.github.io/papers/msc_thesis_memory-safe_network_services_userspace_switch_slides.pdf).
It integrates well with the kernel loopback interface to reach programs on the local IP or be reached by other local programs.
At runtime it accesses the NIC either through macvtap (dedicated NIC or L2 bridge), netmap (dedicated NIC or L2 bridge), or usnetd (L4 switch).
It is intended to be used in combination with [usnetd](https://github.com/ANLAB-KAIST/usnetd), a switch system service that allows to share a NIC and IP address for multiple memory-safe network stacks and the kernel network stack.
This combination easily expands the memory-safety of existing Rust web services to the TCP/IP layer with low porting efforts by just changing the import statements.

Code for evaluation is in the [usnetd](https://github.com/ANLAB-KAIST/usnetd) repository. While 10 GBit/s TCP transfer on a 10G NIC is possible on a fast system, a slower system may have a degraded performance of 2 GBit/s.
The single-thread version of the API could get half of the line-rate when usnetd was not used.
Currently smoltcp does not implement several TCP aspects which would make it more robust against packet loss.
Accepting and answering parallel short TCP connections is not as fast as with the Linux kernel network stack.
See the TODO section for lacking features that some programs may need.
Unsafe Rust code is used for packet transfer with netmap, macvtap syscalls, and file descriptor handover. Read Chapter 3 of the thesis which reasons about the threat model and L2 handling as trusted code base.

Netmap support is an optional build feature. It can be used for direct NIC access or for faster IPC channels to usnetd through netmap pipes instead of Unix domain sockets.

A limitation with macvtap that does not exists with real netmap drivers is that kernel RAW sockets will still see traffic on the interface.
This means that dhclient listening on the real interface may still be exposed to untrusted traffic.
Another problem with macvtap (and the new virtual TAP interface for the kernel if usnetd in macvtap mode is used) is that dhclient and wpasupplicant try to use the real interface where sending is blocked, thus after some time an encrypted WiFi connection may break.
Therefore, usnetd on netmap is the recommended backend.

## Porting Rust code

It is recommendable to introduce support for usnet_sockets through conditional compilation with a build feature flag.
This gives users of your network service the choice if they want to use userspace networking or not (only Linux is supported).

Example ports of Rust libraries which can use usnet_sockets through a `usnet` feature flag are in the `usnet` branches of [tiny-http](https://github.com/pothos/tiny-http) and [rouille](https://github.com/pothos/rouille).

Add a new build feature to your `Cargo.toml` file:

    [features]
    usnet = ["usnet_sockets"]
    
    [dependencies]
    usnet_sockets = { git = "https://github.com/ANLAB-KAIST/usnet_sockets", optional = true }

Then make your imports of the Rust standard library sockets or usnet_sockets depending on the build feature flag in any source code file that imports `std::net::TcpStream` or `std::net::TcpListener`:

    #[cfg(feature = "usnet")]
    use usnet_sockets::{TcpStream, TcpListener};
    #[cfg(not(feature = "usnet"))]
    use std::net::{TcpListener, TcpStream};

Read the section on other build flags if you do not like to use conditional compilation.

If your project just depends on a library that was ported to use `usnet_sockets`, you need to add a build feature flag that enables the build feature flag of the dependency in your `Cargo.toml` file, e.g., assuming you use a ported `tiny_http` library:

    [feature]
    usnet = ["tiny_http/usnet"]
    
    [patch.crates-io]
    tiny_http = { git = "https://../tiny-http" }

This patching also works for dependencies of dependencies, so that you could directly do this in your own project that uses, e.g., the web framework rouille instead of doing this step first for rouille and then add the above step with rouille instead of tiny_http for your project.

If the code uses .to_socket_addrs() before calling into the library then this call will still use the system's resolver.
Therefore, after importing UsnetToSocketAddrs, .usnet_to_socket_addrs() must be used for an internal memory-safe resolution.

## Other build flags of the library
The library has a `host` build flag just reexports the Rust standard library types. This is useful to debug code or avoid conditional compilation when porting some Rust code. However, the library still provides userspace networking types under `usnet_sockets::apimultithread::{TcpListener, TcpStream}`. Only if this would be disabled with maybe a build flag `onlyhost`, a full port to usnet_sockets would not compromise portability to non-Linux systems – this seems to be a good solution but was not implemented yet.

Another build flag is `single` which uses the single-thread version of the socket types by default. It has no background thread and the socket types use no locks and cannot be moved or shared between threads.
The full multithread-capable API is still available as `usnet_sockets::apimultithread::{TcpListener, TcpStream}`.

The build flags need to be used with `--no-default-features` to disable the default `multi` flag.

## Runtime configuration of NIC access
A configuration is required since the final program may share the IP with the kernel, use an L2 bridge, or take over the NIC.
Currently this is done with an environment variable that contains the configuration as JSON serialization. (A TOML file would be a more approachable format but the question is whether it must be specified or a global configuration is automatically chosen.)

Configurations that exclusively take over the NIC from the kernel (dedicated NIC):

    USNET_SOCKETS='{"MacVtapConfig":{"Create":{"mac":"Passthru","parent":{"Interface":"eth0"},"ipv4":"Passthru"}}}'
    # This creates a macvtap interface and if the program is not shut down well, you need to clean it up afterwards with:
    sudo ip link delete eth0pass
    # If you do not want to specify the interface, use "parent":"DiscoverFromRoute" to take the interface with the default route

    # Now with netmap:
    USNET_SOCKETS='{"NetmapConfig":{"Interface":{"interface":{"Interface":{"netmap_name":"netmap:eth0","parent":"eth0"}},"mac":"Passthru","ipv4":"Passthru"}}}'
    # This requires the netmap kernel module, read the README.md file of usnetd for further instructions

Configurations that use a L2 bridge (macvtap bridge or netmap VALE):

    USNET_SOCKETS='{"MacVtapConfig":{"Create":{"mac":{"Static":"aa:bb:cc:dd:ee:00"},"parent":"DiscoverFromRoute","ipv4":{"Static":{"ipv4":"192.168.1.200","sub":24,"gateway":"192.168.1.1"}}}}}'
    # The bridge is created automatically as soon as something else than "Passthru" is used for "mac", i.e., "Random" or the above static config
    # You can also use an existing macvtap device you configured which will not be deconstructed when the program exits:
    USNET_SOCKETS='{"MacVtapConfig":{"Interface:":{"interface":"mymacvtap", "ipv4":{"Static":{"ipv4":"192.168.1.200","sub":24,"gateway":"192.168.1.1"}}}}'
    
    # Now with netmap via VALE switch 0 on port 0:
    USNET_SOCKETS='{"NetmapConfig":{"Interface":{"interface":{"Interface":{"netmap_name":"vale0:0","parent":"eth0"}},"mac":"Random","ipv4":{"Static":{"ipv4":"192.168.1.201","sub":24,"gateway":"192.168.1.1"}} }}}
    # Here using a random MAC


Configurations that use usnetd to share the IP with the kernel and firewall the kernel, this is the **recommended usage**:

    USNET_SOCKETS='{"UsnetConfig":{"Interface":{"interface":{"Interface":"eth0"},"ipc":"NetmapPipe","mac":"Passthru","ipv4":"Passthru"}}}'
    # or "ipc":"UnixDomainSocket", and maybe "interface":"DiscoverFromRoute" as long as usnetd is using the default interface as well

The userspace network stack could also use a static IP and a static MAC when running on usnetd:

    USNET_SOCKETS='{"UsnetConfig":{"Interface":{"interface":{"Interface":"wlan0"},"ipc":"NetmapPipe","mac": {"Static":"aa:bb:cc:dd:ee:07"},"ipv4":{"Static":{"ipv4":"192.168.1.201","sub":24,"gateway":"192.168.1.1"}} }}}'

On usnetd also static netmap pipes (as with VALE) are supported, but this means that no dynamic port management will be used:

    USNET_SOCKETS='{"NetmapConfig":{"Interface":{"interface":{"Interface":{"netmap_name":"netmap:eth0{4094","parent":"eth0"}},"mac":"Passthru","ipv4":"Passthru"}}}'
    # assuming a usnetd configuration with, e.g., STATIC_PIPES=eth0:TCP:8888


Additional environment variables and their defaults are:

    REDUCE_MTU_BY=0 # Sometimes needed because path MTU discovery is not implemented and middleboxes may fail to rewrite smoltcp's TCP header
    SOCKET_BACKLOG=32 # Maximal number of parallel incoming new connections for a listener before one is accepted
    SOCKET_BUFFER=500000 # The maximal TCP receive window
    BG_THREAD_PIN_CPU_ID=-1 # -1 for no CPU core pinning, otherwise the CPU ID where the background thread should run

The socket types will transparently listen on and connect to the loopback interface, i.e., they can interact with applications that use the kernel network stack. This behavior cannot yet be disabled through a configuration variable.

# TODO

* Configurable DNS servers
* UDP broadcast, multicast, configurable max packet number for buffer, and zero-copy variants for recv_from, send_to, peak_from, send, recv, peek
* IPv6, and then run all tests from https://github.com/rust-lang/rust/blob/master/src/libstd/net/tcp.rs
* See smoltcp list of unimplemented features (congestion control, IP fragmentation, path MTU discovery, probing zero windows, DHCP, selective/delayed ACKs, avoiding silly window syndrome, Nagle's algorithm, …)
* Multiple IP addresses per NIC
* Multiple NICs and routing
* Support epoll for porting mio/Tokio and provide a custom RawFd type (conversion to and from RawFds however should still not be possible)
* Better multithreading usage: Fine-grained locking for smoltcp, optimized unblocking of application threads, multiple background threads
* Deregister port matches on drop/prune
