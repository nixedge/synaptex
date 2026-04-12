// synaptex_hook.cc — Kea pkt4_receive hook shim
//
// Three responsibilities:
//
// 1. In-memory host data source (SynaptexMemoryHostDataSource):
//    Registers a custom BaseHostDataSource backend with Kea's HostMgr so
//    that dynamic host reservations pushed by synaptex-router are visible
//    to Kea's address allocation engine without any external database.
//
// 2. DHCP classification (pkt4_receive, one connection per Kea worker thread):
//    Connects to synaptex-router's Unix socket, forwards packet metadata,
//    and applies the returned client-class names to the packet.
//
// 3. Reservation command channel (one persistent connection at load time):
//    Makes a second connection to the same socket with {"type":"cmd"} as the
//    first message.  synaptex-router then pushes reservation-add/del commands
//    over that connection; the hook handles them via Kea's HostMgr, which
//    dispatches into our in-memory data source.
//
// Configuration (kea-dhcp4.conf hooks-libraries entry):
//   {
//     "library": "/path/to/synaptex_hook.so",
//     "parameters": { "socket": "/run/synaptex-router/kea-hook.sock" }
//   }
//
// If synaptex-router is unavailable the shim silently passes through.
// The cmd channel reconnects automatically every 5 s until the router is up.

#include <hooks/hooks.h>
#include <dhcp/pkt4.h>
#include <dhcp/dhcp4.h>
#include <cc/data.h>
#include <asiolink/io_address.h>
#include <dhcpsrv/host_mgr.h>
#include <dhcpsrv/host.h>
#include <dhcpsrv/base_host_data_source.h>
#include <dhcpsrv/host_data_source_factory.h>
#include <dhcpsrv/subnet_id.h>
#include <boost/make_shared.hpp>

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <cstring>
#include <cerrno>
#include <cstdio>
#include <iomanip>
#include <sstream>
#include <string>
#include <vector>
#include <unordered_map>
#include <thread>
#include <mutex>
#include <atomic>
#include <chrono>

using namespace isc::hooks;
using namespace isc::dhcp;
using namespace isc::data;
using namespace isc::asiolink;

// ─── Configuration ────────────────────────────────────────────────────────────

static std::string g_socket_path;

// Hard timeout for the classification round-trip (milliseconds).
// DHCP clients retry after 4 s so 50 ms is safe.
static constexpr int TIMEOUT_MS = 50;

// ─── In-memory host data source ───────────────────────────────────────────────
//
// Registered with Kea's HostDataSourceFactory so HostMgr::add() has a
// writable backend and Kea's allocator finds our reservations at runtime.
// Storage is a simple MAC→Host map protected by a mutex.

static std::string make_mac_key(const uint8_t* begin, size_t len) {
    char buf[3];
    std::string key;
    key.reserve(len * 2);
    for (size_t i = 0; i < len && i < 6; ++i) {
        snprintf(buf, sizeof(buf), "%02x", begin[i]);
        key += buf;
    }
    return key;
}

class SynaptexMemoryHostDataSource : public BaseHostDataSource {
    mutable std::mutex mutex_;
    std::unordered_map<std::string, HostPtr> by_mac_;

public:
    // ── Required metadata ─────────────────────────────────────────────────────

    std::string getType() const override { return "synaptex-memory"; }

    bool setIPReservationsUnique(const bool) override { return true; }

    // ── Write operations ──────────────────────────────────────────────────────

    void add(const HostPtr& host) override {
        if (!host) return;
        // Keep hw_ptr alive — getHWAddress() returns a temporary shared_ptr, so
        // binding a reference to ->hwaddr_ without storing it would be a
        // dangling reference once the temporary is destroyed at the semicolon.
        HWAddrPtr hw_ptr = host->getHWAddress();
        if (!hw_ptr || hw_ptr->hwaddr_.empty()) return;
        const auto& hw = hw_ptr->hwaddr_;
        std::string key = make_mac_key(hw.data(), hw.size());
        std::lock_guard<std::mutex> lk(mutex_);
        by_mac_[key] = host;  // natural upsert
    }

    bool del(const SubnetID& subnet_id, const IOAddress& addr) override {
        std::lock_guard<std::mutex> lk(mutex_);
        for (auto it = by_mac_.begin(); it != by_mac_.end(); ++it) {
            if (it->second->getIPv4SubnetID() == subnet_id &&
                it->second->getIPv4Reservation() == addr) {
                by_mac_.erase(it);
                return true;
            }
        }
        return false;
    }

    bool del4(const SubnetID& subnet_id,
              const Host::IdentifierType& id_type,
              const uint8_t* id_begin,
              const size_t id_len) override {
        if (id_type != Host::IDENT_HWADDR) return false;
        std::string key = make_mac_key(id_begin, id_len);
        std::lock_guard<std::mutex> lk(mutex_);
        auto it = by_mac_.find(key);
        if (it != by_mac_.end() && it->second->getIPv4SubnetID() == subnet_id) {
            by_mac_.erase(it);
            return true;
        }
        return false;
    }

    bool del6(const SubnetID&,
              const Host::IdentifierType&,
              const uint8_t*,
              const size_t) override {
        return false;
    }

    // ── Read operations ───────────────────────────────────────────────────────

    ConstHostPtr
    get4(const SubnetID& subnet_id,
         const Host::IdentifierType& id_type,
         const uint8_t* id_begin,
         const size_t id_len) const override {
        if (id_type != Host::IDENT_HWADDR) return {};
        std::string key = make_mac_key(id_begin, id_len);
        std::lock_guard<std::mutex> lk(mutex_);
        auto it = by_mac_.find(key);
        if (it != by_mac_.end() && it->second->getIPv4SubnetID() == subnet_id) {
            return it->second;
        }
        return {};
    }

    ConstHostPtr
    get4(const SubnetID& subnet_id,
         const IOAddress& addr) const override {
        std::lock_guard<std::mutex> lk(mutex_);
        for (const auto& kv : by_mac_) {
            if (kv.second->getIPv4SubnetID() == subnet_id &&
                kv.second->getIPv4Reservation() == addr) {
                return kv.second;
            }
        }
        return {};
    }

    ConstHostCollection
    getAll4(const SubnetID& subnet_id,
            const IOAddress& addr) const override {
        ConstHostCollection result;
        std::lock_guard<std::mutex> lk(mutex_);
        for (const auto& kv : by_mac_) {
            if (kv.second->getIPv4SubnetID() == subnet_id &&
                kv.second->getIPv4Reservation() == addr) {
                result.push_back(kv.second);
            }
        }
        return result;
    }

    ConstHostCollection
    getAll4(const IOAddress& addr) const override {
        ConstHostCollection result;
        std::lock_guard<std::mutex> lk(mutex_);
        for (const auto& kv : by_mac_) {
            if (kv.second->getIPv4Reservation() == addr) {
                result.push_back(kv.second);
            }
        }
        return result;
    }

    ConstHostCollection
    getAll(const Host::IdentifierType& id_type,
           const uint8_t* id_begin,
           const size_t id_len) const override {
        ConstHostCollection result;
        if (id_type != Host::IDENT_HWADDR) return result;
        std::string key = make_mac_key(id_begin, id_len);
        std::lock_guard<std::mutex> lk(mutex_);
        auto it = by_mac_.find(key);
        if (it != by_mac_.end()) result.push_back(it->second);
        return result;
    }

    // ── Stubs for unused IPv6/hostname/paging methods ─────────────────────────

    ConstHostCollection
    getAll4(const SubnetID&) const override { return {}; }

    ConstHostCollection
    getAll6(const SubnetID&) const override { return {}; }

    ConstHostCollection
    getAllbyHostname(const std::string&) const override { return {}; }

    ConstHostCollection
    getAllbyHostname4(const std::string&, const SubnetID&) const override { return {}; }

    ConstHostCollection
    getAllbyHostname6(const std::string&, const SubnetID&) const override { return {}; }

    ConstHostCollection
    getPage4(const SubnetID&, size_t& source_index, uint64_t,
             const HostPageSize&) const override {
        source_index = std::numeric_limits<size_t>::max();
        return {};
    }

    ConstHostCollection
    getPage6(const SubnetID&, size_t& source_index, uint64_t,
             const HostPageSize&) const override {
        source_index = std::numeric_limits<size_t>::max();
        return {};
    }

    ConstHostCollection
    getPage4(size_t& source_index, uint64_t,
             const HostPageSize&) const override {
        source_index = std::numeric_limits<size_t>::max();
        return {};
    }

    ConstHostCollection
    getPage6(size_t& source_index, uint64_t,
             const HostPageSize&) const override {
        source_index = std::numeric_limits<size_t>::max();
        return {};
    }

    ConstHostPtr
    get6(const SubnetID&, const Host::IdentifierType&,
         const uint8_t*, const size_t) const override { return {}; }

    ConstHostPtr
    get6(const IOAddress&, const uint8_t) const override { return {}; }

    ConstHostPtr
    get6(const SubnetID&, const IOAddress&) const override { return {}; }

    ConstHostCollection
    getAll6(const SubnetID&, const IOAddress&) const override { return {}; }

    ConstHostCollection
    getAll6(const IOAddress&) const override { return {}; }
};

static boost::shared_ptr<SynaptexMemoryHostDataSource> g_host_source;

// ─── Reservation command channel ─────────────────────────────────────────────
//
// One persistent connection to synaptex-router, opened at load() time.
// synaptex-router pushes reservation-add/del commands over it; we handle
// them via Kea's HostMgr (which dispatches into our in-memory data source)
// and write back a one-line JSON result.

static std::thread             g_cmd_thread;
static std::atomic<bool>       g_cmd_running{false};
static std::mutex              g_shutdown_mutex;
static std::condition_variable g_shutdown_cv;

// Connect to synaptex-router and set a 1-second receive timeout so that
// read() in cmd_channel_loop returns EAGAIN periodically rather than
// blocking forever, allowing the thread to check g_cmd_running and exit
// promptly when unload() fires.
static int connect_to_router() {
    int fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;

    struct timeval tv{1, 0};  // 1-second read timeout
    ::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

    struct sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    ::strncpy(addr.sun_path, g_socket_path.c_str(), sizeof(addr.sun_path) - 1);
    if (::connect(fd, reinterpret_cast<struct sockaddr*>(&addr), sizeof(addr)) < 0) {
        ::close(fd);
        return -1;
    }
    return fd;
}

// Interruptible reconnect delay: waits up to 5 seconds but wakes immediately
// when unload() calls g_shutdown_cv.notify_all().
static void reconnect_wait() {
    std::unique_lock<std::mutex> lk(g_shutdown_mutex);
    g_shutdown_cv.wait_for(lk, std::chrono::seconds(5),
                           [] { return !g_cmd_running.load(); });
}

static std::vector<uint8_t> parse_mac_bytes(const std::string& mac) {
    std::vector<uint8_t> out;
    out.reserve(6);
    std::istringstream ss(mac);
    std::string tok;
    while (std::getline(ss, tok, ':')) {
        out.push_back(static_cast<uint8_t>(std::stoi(tok, nullptr, 16)));
    }
    return out;
}

static std::string handle_reservation_add(ConstElementPtr args) {
    auto mac_e    = args->get("mac");
    auto ip_e     = args->get("ip");
    auto subnet_e = args->get("subnet_id");
    if (!mac_e || !ip_e || !subnet_e) {
        return "{\"result\":1,\"text\":\"missing mac/ip/subnet_id\"}\n";
    }
    try {
        auto mac  = parse_mac_bytes(mac_e->stringValue());
        auto host = boost::make_shared<Host>(
            mac.data(), mac.size(),
            Host::IDENT_HWADDR,
            SubnetID(subnet_e->intValue()),
            SUBNET_ID_UNUSED,
            isc::asiolink::IOAddress(ip_e->stringValue())
        );
        // Write directly to g_host_source — add() is a natural upsert by MAC
        // key so no duplicate-check is needed.  We bypass HostMgr::add() here
        // because HostMgr::add() throws "no hosts-database configured" when its
        // alternate_sources_ list is empty, which happens whenever Kea's config
        // parser calls HostMgr::create() and resets it after load() runs.
        g_host_source->add(host);
        return "{\"result\":0,\"text\":\"ok\"}\n";
    } catch (const std::exception& e) {
        return std::string("{\"result\":1,\"text\":\"") + e.what() + "\"}\n";
    }
}

static std::string handle_reservation_del(ConstElementPtr args) {
    auto mac_e    = args->get("mac");
    auto subnet_e = args->get("subnet_id");
    if (!mac_e || !subnet_e) {
        return "{\"result\":1,\"text\":\"missing mac/subnet_id\"}\n";
    }
    try {
        auto mac = parse_mac_bytes(mac_e->stringValue());
        // Write directly to g_host_source for the same reason as add: avoid
        // HostMgr::del4() which also guards on alternate_sources_ being set.
        g_host_source->del4(
            SubnetID(subnet_e->intValue()),
            Host::IDENT_HWADDR,
            mac.data(), mac.size()
        );
        return "{\"result\":0,\"text\":\"ok\"}\n";
    } catch (const std::exception& e) {
        return std::string("{\"result\":1,\"text\":\"") + e.what() + "\"}\n";
    }
}

static std::string dispatch_cmd(const std::string& line) {
    try {
        ConstElementPtr msg = Element::fromJSON(line);
        auto cmd_e = msg->get("cmd");
        if (!cmd_e) return "{\"result\":1,\"text\":\"missing cmd\"}\n";
        std::string cmd = cmd_e->stringValue();
        if (cmd == "reservation-add") return handle_reservation_add(msg);
        if (cmd == "reservation-del") return handle_reservation_del(msg);
        return "{\"result\":1,\"text\":\"unknown cmd\"}\n";
    } catch (const std::exception& e) {
        return std::string("{\"result\":1,\"text\":\"parse error: ") + e.what() + "\"}\n";
    }
}

// Persistent cmd channel loop: connects (and reconnects) to synaptex-router,
// announces itself as a cmd channel, then handles incoming reservation commands.
//
// Shutdown contract: unload() sets g_cmd_running=false and notifies
// g_shutdown_cv.  The SO_RCVTIMEO on the socket (1 s) bounds how long read()
// can block, so join() in unload() completes within ~1 second.  The
// reconnect_wait() helper also wakes immediately on notify.
static void cmd_channel_loop() {
    while (g_cmd_running) {
        int fd = connect_to_router();
        if (fd < 0) {
            reconnect_wait();
            continue;
        }

        // Announce cmd channel to synaptex-router.
        const char* init = "{\"type\":\"cmd\"}\n";
        if (::write(fd, init, ::strlen(init)) < 0) {
            ::close(fd);
            reconnect_wait();
            continue;
        }

        fprintf(stderr, "synaptex_hook: cmd channel connected\n");

        // Read newline-delimited JSON commands until disconnected.
        // read() returns EAGAIN after SO_RCVTIMEO (1 s); we re-check
        // g_cmd_running and continue so unload() can join promptly.
        std::string buf;
        char ch;
        while (g_cmd_running) {
            ssize_t n = ::read(fd, &ch, 1);
            if (n < 0) {
                if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) {
                    continue;  // timeout or signal — check g_cmd_running
                }
                break;  // real error
            }
            if (n == 0) break;  // peer closed
            if (ch == '\n') {
                if (!buf.empty()) {
                    std::string resp = dispatch_cmd(buf);
                    ::write(fd, resp.c_str(), resp.size());
                    buf.clear();
                }
            } else {
                buf += ch;
            }
        }

        fprintf(stderr, "synaptex_hook: cmd channel disconnected, reconnecting...\n");
        ::close(fd);

        if (g_cmd_running) {
            reconnect_wait();
        }
    }
}

// ─── Per-thread persistent socket (DHCP classification) ──────────────────────

struct ThreadSocket {
    int fd = -1;

    ~ThreadSocket() { close_fd(); }

    void close_fd() {
        if (fd >= 0) { ::close(fd); fd = -1; }
    }

    bool connected() const { return fd >= 0; }

    bool connect() {
        close_fd();
        fd = ::socket(AF_UNIX, SOCK_STREAM, 0);
        if (fd < 0) return false;

        struct timeval tv{};
        tv.tv_sec  = TIMEOUT_MS / 1000;
        tv.tv_usec = (TIMEOUT_MS % 1000) * 1000;
        ::setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
        ::setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv));

        struct sockaddr_un addr{};
        addr.sun_family = AF_UNIX;
        ::strncpy(addr.sun_path, g_socket_path.c_str(), sizeof(addr.sun_path) - 1);

        if (::connect(fd, reinterpret_cast<struct sockaddr*>(&addr), sizeof(addr)) < 0) {
            close_fd();
            return false;
        }
        return true;
    }

    bool send_all(const std::string& data) {
        const char* p = data.data();
        size_t left = data.size();
        while (left > 0) {
            ssize_t n = ::write(fd, p, left);
            if (n <= 0) return false;
            p += n; left -= n;
        }
        return true;
    }

    std::string read_line() {
        std::string line;
        char c;
        while (true) {
            ssize_t n = ::read(fd, &c, 1);
            if (n <= 0) return "";
            if (c == '\n') return line;
            line += c;
        }
    }
};

static thread_local ThreadSocket tls_sock;

// ─── Helpers ──────────────────────────────────────────────────────────────────

static std::string mac_to_str(const HWAddrPtr& hw) {
    if (!hw || hw->hwaddr_.empty()) return "";
    std::ostringstream oss;
    for (size_t i = 0; i < hw->hwaddr_.size(); ++i) {
        if (i > 0) oss << ':';
        oss << std::hex << std::setw(2) << std::setfill('0')
            << static_cast<unsigned>(hw->hwaddr_[i]);
    }
    return oss.str();
}

static std::string json_str(const std::string& s) {
    std::string out;
    out.reserve(s.size() + 2);
    out += '"';
    for (char c : s) {
        if (c == '"' || c == '\\') out += '\\';
        out += c;
    }
    out += '"';
    return out;
}

static std::string build_request(const Pkt4Ptr& pkt) {
    std::ostringstream j;
    j << "{\"mac\":"      << json_str(mac_to_str(pkt->getHWAddr()));
    j << ",\"msg_type\":" << static_cast<unsigned>(pkt->getType());

    auto giaddr = pkt->getGiaddr();
    if (!giaddr.isV4Zero()) {
        j << ",\"giaddr\":" << json_str(giaddr.toText());
    }

    auto opt12 = pkt->getOption(DHO_HOST_NAME);
    if (opt12) {
        auto& d = opt12->getData();
        j << ",\"hostname\":" << json_str(std::string(d.begin(), d.end()));
    }

    auto opt60 = pkt->getOption(DHO_VENDOR_CLASS_IDENTIFIER);
    if (opt60) {
        auto& d = opt60->getData();
        j << ",\"vendor_class\":" << json_str(std::string(d.begin(), d.end()));
    }

    auto opt55 = pkt->getOption(DHO_DHCP_PARAMETER_REQUEST_LIST);
    if (opt55) {
        auto& d = opt55->getData();
        j << ",\"prl\":[";
        for (size_t i = 0; i < d.size(); ++i) {
            if (i > 0) j << ',';
            j << static_cast<unsigned>(d[i]);
        }
        j << ']';
    }

    j << "}\n";
    return j.str();
}

static std::vector<std::string> parse_classes(const std::string& json) {
    std::vector<std::string> out;
    auto pos = json.find("\"classes\"");
    if (pos == std::string::npos) return out;
    pos = json.find('[', pos);
    if (pos == std::string::npos) return out;
    while (true) {
        pos = json.find('"', pos + 1);
        if (pos == std::string::npos) break;
        auto end = json.find('"', pos + 1);
        if (end == std::string::npos) break;
        out.push_back(json.substr(pos + 1, end - pos - 1));
        pos = end + 1;
        auto next = json.find_first_not_of(" \t,", pos);
        if (next == std::string::npos || json[next] == ']') break;
    }
    return out;
}

// ─── Hook entry points ────────────────────────────────────────────────────────

extern "C" {

int version() {
    return KEA_HOOKS_VERSION;
}

int multi_threading_compatible() {
    return 1;
}

int load(LibraryHandle& handle) {
    ConstElementPtr params = handle.getParameters();
    if (!params) {
        fprintf(stderr, "synaptex_hook: 'socket' parameter required\n");
        return 1;
    }
    ConstElementPtr socket_elem = params->get("socket");
    if (!socket_elem || socket_elem->getType() != Element::string) {
        fprintf(stderr, "synaptex_hook: 'socket' must be a string\n");
        return 1;
    }
    g_socket_path = socket_elem->stringValue();

    // Register our in-memory host data source so HostMgr::add() has a
    // writable backend and Kea's allocator can find our reservations.
    auto mem_source = boost::make_shared<SynaptexMemoryHostDataSource>();
    g_host_source   = mem_source;

    HostDataSourceFactory::registerFactory(
        "synaptex-memory",
        [mem_source](const isc::db::DatabaseConnection::ParameterMap&)
            -> HostDataSourcePtr {
            return mem_source;
        }
    );
    HostMgr::instance().addBackend("type=synaptex-memory");

    // Start the persistent cmd channel thread.  It connects (and reconnects)
    // to synaptex-router in the background so load() returns immediately.
    g_cmd_running = true;
    g_cmd_thread  = std::thread(cmd_channel_loop);

    fprintf(stderr, "synaptex_hook: loaded, socket=%s\n", g_socket_path.c_str());
    return 0;
}

int unload() {
    g_cmd_running = false;
    g_shutdown_cv.notify_all();  // wake reconnect_wait() if the thread is sleeping
    if (g_cmd_thread.joinable()) {
        g_cmd_thread.join();     // completes within ~1 s (SO_RCVTIMEO bound)
    }

    HostMgr::instance().delBackend("synaptex-memory");
    HostDataSourceFactory::deregisterFactory("synaptex-memory");
    g_host_source.reset();

    return 0;
}

// Called by Kea after it has finished parsing the DHCP configuration and
// (re-)creating HostMgr from the "hosts-databases" section (or from nothing).
// HostMgr::create() clears alternate_sources_, so any addBackend() call made
// in load() is wiped out.  We re-add our backend here, after the reset, so
// Kea's allocator can iterate it during get4() lookups.
int dhcp4_srv_configured(CalloutHandle& /*handle*/) {
    try {
        HostMgr::instance().addBackend("type=synaptex-memory");
        fprintf(stderr, "synaptex_hook: host backend registered after srv configured\n");
    } catch (const std::exception& e) {
        // Log but don't fail — if it's already registered Kea may throw.
        fprintf(stderr, "synaptex_hook: addBackend in dhcp4_srv_configured: %s\n", e.what());
    }
    return 0;
}

int pkt4_receive(CalloutHandle& handle) {
    Pkt4Ptr pkt4;
    handle.getArgument("query4", pkt4);
    if (!pkt4) return 0;

    if (!tls_sock.connected() && !tls_sock.connect()) {
        return 0;
    }

    std::string req = build_request(pkt4);

    if (!tls_sock.send_all(req)) {
        if (!tls_sock.connect() || !tls_sock.send_all(req)) {
            return 0;
        }
    }

    std::string resp = tls_sock.read_line();
    if (resp.empty()) {
        tls_sock.close_fd();
        return 0;
    }

    for (const auto& cls : parse_classes(resp)) {
        pkt4->addClass(cls);
    }

    return 0;
}

}  // extern "C"
