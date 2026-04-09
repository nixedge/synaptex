// synaptex_hook.cc — Kea pkt4_receive hook shim
//
// Forwards DHCP packet metadata to synaptex-router via a persistent Unix
// domain socket and applies the returned client-class names to the packet.
//
// Configuration (kea-dhcp4.conf hooks-libraries entry):
//   {
//     "library": "/path/to/synaptex_hook.so",
//     "parameters": { "socket": "/run/synaptex/kea-hook.sock" }
//   }
//
// If synaptex-router is unavailable the shim silently passes through with no
// classes set, so Kea continues with its statically-configured subnet rules.

#include <hooks/hooks.h>
#include <dhcp/pkt4.h>
#include <dhcp/dhcp4.h>
#include <cc/data.h>
#include <asiolink/io_address.h>

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

using namespace isc::hooks;
using namespace isc::dhcp;
using namespace isc::data;

// ─── Configuration ────────────────────────────────────────────────────────────

static std::string g_socket_path;

// Hard timeout for the round-trip to synaptex-router (milliseconds).
// DHCP clients retry after 4 s so 50 ms is safe.
static constexpr int TIMEOUT_MS = 50;

// ─── Per-thread persistent socket ─────────────────────────────────────────────
//
// Each Kea worker thread gets its own connection.  Thread-local storage means
// no locking needed; the destructor closes the fd when the thread exits.

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

        // Apply send/recv timeout so we never block Kea indefinitely.
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

    // Read until newline.  Returns empty string on timeout or error.
    std::string read_line() {
        std::string line;
        char c;
        while (true) {
            ssize_t n = ::read(fd, &c, 1);
            if (n <= 0) return "";  // timeout (EAGAIN) or error
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

// Minimal parser for {"classes":["A","B"]}.
// No external JSON library — keeps the shim dependency-free.
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
        // Stop at ']'
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
    fprintf(stderr, "synaptex_hook: loaded, socket=%s\n", g_socket_path.c_str());
    return 0;
}

int unload() {
    // tls_sock cleaned up per-thread automatically.
    return 0;
}

int pkt4_receive(CalloutHandle& handle) {
    Pkt4Ptr pkt4;
    handle.getArgument("query4", pkt4);
    if (!pkt4) return 0;

    // Connect (or reconnect) if needed.
    if (!tls_sock.connected() && !tls_sock.connect()) {
        return 0;  // router unavailable — pass through
    }

    std::string req = build_request(pkt4);

    // Send; retry once on failure (e.g. router restarted).
    if (!tls_sock.send_all(req)) {
        if (!tls_sock.connect() || !tls_sock.send_all(req)) {
            return 0;
        }
    }

    std::string resp = tls_sock.read_line();
    if (resp.empty()) {
        // Timeout or broken pipe — drop connection so next call reconnects.
        tls_sock.close_fd();
        return 0;
    }

    for (const auto& cls : parse_classes(resp)) {
        pkt4->addClass(cls);
    }

    return 0;  // NEXT_STEP_CONTINUE
}

}  // extern "C"
