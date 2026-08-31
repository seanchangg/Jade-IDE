// Fixed Verilator harness for the jade board simulation.
//
// This file never changes per project. The generated `jade_hw_top.v` wrapper
// absorbs all per-project variance and always exposes the fixed port set
// (clk, pb[3:0], dipsw[4:0], led[4:0]).
//
// Protocol v1 over stdio. See jade-hw/src/protocol.rs for the grammar.
// t_ns = cycles * 20 (one 50 MHz clock period is 20 ns).
//
// Time model:
// - One slice is at most 50,000 cycles (1 ms of simulated time).
// - The schedule is drift free: target_wall = anchor + cycles / rate.
// - When the harness runs behind, the debt is clamped to 100 ms, so a stall
//   causes a skip, not a burst.
// - Paused state blocks on stdin and uses zero CPU.

#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>

#include <poll.h>
#include <unistd.h>

#include <chrono>

#include "Vjade_hw_top.h"
#include "verilated.h"

namespace {

constexpr double kClockHz = 50e6;
constexpr uint64_t kSliceCycles = 50000;      // 1 ms of simulated time
constexpr double kMaxDebtSec = 0.100;         // behind-schedule clamp
constexpr double kLedWindowSec = 0.033;       // LED / DUTY emission window
constexpr double kRateReportSec = 1.0;
constexpr int64_t kSampleStride = 4;          // duty decimation

double now_sec() {
    using namespace std::chrono;
    return duration<double>(steady_clock::now().time_since_epoch()).count();
}

struct Harness {
    Vjade_hw_top* top;
    bool quit = false;
    std::string quit_reason = "quit";
    bool paused = false;

    // Commanded rate: 0 = real-time 50 MHz, >0 = cycles per second,
    // -1 = as fast as possible.
    int64_t rate_hz = 0;

    uint64_t total_cycles = 0;

    // Drift-free schedule anchor.
    double anchor_wall = 0.0;
    uint64_t anchor_cycles = 0;

    // LED emission state.
    uint8_t last_led_mask = 0xFF;   // force the first emission
    double last_led_emit = 0.0;

    // Duty window accumulators (decimated sampling).
    uint64_t duty_high[5] = {0, 0, 0, 0, 0};
    uint64_t duty_samples = 0;
    double window_start = 0.0;

    // Rate report.
    uint64_t rate_cycles_mark = 0;
    double rate_wall_mark = 0.0;

    // Stdin line buffer.
    std::string in_buf;

    uint64_t t_ns() const { return total_cycles * 20; }

    void emit(const char* fmt, ...) {
        va_list ap;
        va_start(ap, fmt);
        vprintf(fmt, ap);
        va_end(ap);
        putchar('\n');
        fflush(stdout);
    }

    void emit_state() {
        emit("__JADE_HW|STATE|%s", paused ? "paused" : "running");
    }

    void re_anchor() {
        anchor_wall = now_sec();
        anchor_cycles = total_cycles;
    }

    double cycles_per_sec() const {
        if (rate_hz > 0) return (double)rate_hz;
        return kClockHz;  // rate 0: real time
    }

    bool slow_motion() const { return rate_hz > 0 && rate_hz <= 10; }

    // Run n clock cycles. Sample LED duty every kSampleStride cycles.
    void run_cycles(uint64_t n) {
        const bool edges = slow_motion();
        for (uint64_t i = 0; i < n; i++) {
            top->clk = 0;
            top->eval();
            if (edges) emit("__JADE_HW|CLK|%llu|0", (unsigned long long)t_ns());
            top->clk = 1;
            top->eval();
            if (edges) emit("__JADE_HW|CLK|%llu|1", (unsigned long long)t_ns());
            total_cycles++;
            if ((total_cycles & (kSampleStride - 1)) == 0) {
                uint8_t led = top->led & 0x1F;
                duty_high[0] += (led >> 0) & 1;
                duty_high[1] += (led >> 1) & 1;
                duty_high[2] += (led >> 2) & 1;
                duty_high[3] += (led >> 3) & 1;
                duty_high[4] += (led >> 4) & 1;
                duty_samples++;
            }
        }
    }

    // Emit LED (on change), DUTY (per window), and RATE (per second).
    void maybe_emit(bool force_led) {
        double now = now_sec();
        uint8_t mask = top->led & 0x1F;
        if (force_led ||
            (mask != last_led_mask && now - last_led_emit >= kLedWindowSec)) {
            emit("__JADE_HW|LED|%llu|%u", (unsigned long long)t_ns(), mask);
            last_led_mask = mask;
            last_led_emit = now;
        }
        if (now - window_start >= kLedWindowSec) {
            if (duty_samples > 0) {
                unsigned d[5];
                for (int i = 0; i < 5; i++) {
                    d[i] = (unsigned)((duty_high[i] * 255) / duty_samples);
                    duty_high[i] = 0;
                }
                emit("__JADE_HW|DUTY|%llu|%u,%u,%u,%u,%u",
                     (unsigned long long)t_ns(), d[0], d[1], d[2], d[3], d[4]);
                duty_samples = 0;
            }
            window_start = now;
        }
        if (now - rate_wall_mark >= kRateReportSec) {
            double dt = now - rate_wall_mark;
            double cps = (double)(total_cycles - rate_cycles_mark) / dt;
            uint64_t slow = (uint64_t)(cps / kClockHz * 1000.0);
            emit("__JADE_HW|RATE|%llu|%.0f|%llu", (unsigned long long)t_ns(),
                 cps, (unsigned long long)slow);
            rate_cycles_mark = total_cycles;
            rate_wall_mark = now;
        }
    }

    void handle_command(const std::string& line) {
        std::vector<std::string> parts;
        size_t pos = 0;
        while (true) {
            size_t bar = line.find('|', pos);
            if (bar == std::string::npos) {
                parts.push_back(line.substr(pos));
                break;
            }
            parts.push_back(line.substr(pos, bar - pos));
            pos = bar + 1;
        }
        const std::string& cmd = parts[0];
        if (cmd == "QUIT") {
            quit = true;
            quit_reason = "quit";
        } else if (cmd == "PAUSE") {
            if (!paused) {
                paused = true;
                emit_state();
            }
        } else if (cmd == "RESUME") {
            if (paused) {
                paused = false;
                re_anchor();
                emit_state();
            }
        } else if (cmd == "STEP" && parts.size() >= 2) {
            uint64_t n = strtoull(parts[1].c_str(), nullptr, 10);
            if (!paused) {
                paused = true;
            }
            run_cycles(n);
            maybe_emit(true);
            emit("__JADE_HW|STATE|stepped");
        } else if (cmd == "RATE" && parts.size() >= 2) {
            rate_hz = strtoll(parts[1].c_str(), nullptr, 10);
            re_anchor();
        } else if (cmd == "SET" && parts.size() >= 4) {
            int idx = atoi(parts[2].c_str());
            int level = atoi(parts[3].c_str()) ? 1 : 0;
            if (parts[1] == "PB" && idx >= 0 && idx < 4) {
                uint8_t v = top->pb;
                v = (uint8_t)((v & ~(1u << idx)) | ((unsigned)level << idx));
                top->pb = v & 0xF;
            } else if (parts[1] == "DIPSW" && idx >= 0 && idx < 5) {
                uint8_t v = top->dipsw;
                v = (uint8_t)((v & ~(1u << idx)) | ((unsigned)level << idx));
                top->dipsw = v & 0x1F;
            }
        } else if (cmd == "SETALL" && parts.size() >= 3) {
            if (parts[1] == "DIPSW") {
                top->dipsw = (uint8_t)(atoi(parts[2].c_str()) & 0x1F);
            }
        }
        // Unknown commands are ignored.
    }

    // Poll stdin for up to timeout_ms (-1 blocks). Process complete lines.
    // Returns false on EOF.
    bool pump_stdin(int timeout_ms) {
        struct pollfd pfd = {0, POLLIN, 0};
        int r = poll(&pfd, 1, timeout_ms);
        if (r <= 0) return true;
        if (pfd.revents & (POLLIN | POLLHUP)) {
            char buf[4096];
            ssize_t n = read(0, buf, sizeof(buf));
            if (n <= 0) {
                quit = true;
                quit_reason = "eof";
                return false;
            }
            in_buf.append(buf, (size_t)n);
            size_t nl;
            while ((nl = in_buf.find('\n')) != std::string::npos) {
                std::string line = in_buf.substr(0, nl);
                in_buf.erase(0, nl + 1);
                if (!line.empty() && line.back() == '\r') line.pop_back();
                if (!line.empty()) handle_command(line);
            }
        }
        return true;
    }
};

}  // namespace

int main(int argc, char** argv) {
    Verilated::commandArgs(argc, argv);
    Harness h;
    h.top = new Vjade_hw_top;

    // Electrical rest state: buttons not pushed (high), DIP switches in the
    // OFF position (high). The session replays real state after a hot swap.
    h.top->pb = 0xF;
    h.top->dipsw = 0x1F;
    h.top->clk = 0;
    h.top->eval();

    // The build passes -DJADE_HW_USER_TOP=<name>; stringify it here so the
    // compile command needs no shell quoting.
#ifndef JADE_HW_USER_TOP
#define JADE_HW_USER_TOP unknown
#endif
#define JADE_HW_STR2(x) #x
#define JADE_HW_STR(x) JADE_HW_STR2(x)
    h.emit("__JADE_HW|HELLO|1|%s", JADE_HW_STR(JADE_HW_USER_TOP));
    h.emit("__JADE_HW|PORTS|clk:in:1,pb:in:4,dipsw:in:5,led:out:5");
    h.emit_state();

    double start = now_sec();
    h.anchor_wall = start;
    h.window_start = start;
    h.rate_wall_mark = start;
    h.last_led_emit = start - kLedWindowSec;

    while (!h.quit) {
        if (h.paused) {
            // Zero CPU while paused: block until a command arrives.
            h.pump_stdin(-1);
            continue;
        }
        h.pump_stdin(0);
        if (h.quit) break;
        if (h.paused) continue;

        if (h.rate_hz < 0) {
            // Maximum speed: no pacing.
            h.run_cycles(kSliceCycles);
            h.maybe_emit(false);
            continue;
        }

        double cps = h.cycles_per_sec();
        double now = now_sec();
        double elapsed = now - h.anchor_wall;
        uint64_t due_total = (uint64_t)(elapsed * cps);
        uint64_t done = h.total_cycles - h.anchor_cycles;
        if (due_total <= done) {
            // Ahead of schedule: wait for the next cycle or a command.
            double next = h.anchor_wall + (double)(done + 1) / cps;
            double wait = next - now;
            int ms = (int)(wait * 1000.0);
            if (ms < 1) ms = 1;
            if (ms > 33) ms = 33;
            h.pump_stdin(ms);
            h.maybe_emit(false);
            continue;
        }
        uint64_t behind = due_total - done;
        double debt_sec = (double)behind / cps;
        if (debt_sec > kMaxDebtSec) {
            // Too far behind: forgive the debt beyond the clamp, so a stall
            // causes a skip, not a burst.
            h.anchor_wall = now - (double)done / cps - kMaxDebtSec;
            behind = (uint64_t)(kMaxDebtSec * cps);
            if (behind == 0) behind = 1;
        }
        if (behind > kSliceCycles) behind = kSliceCycles;
        h.run_cycles(behind);
        h.maybe_emit(false);
    }

    h.top->final();
    h.emit("__JADE_HW|BYE|%s", h.quit_reason.c_str());
    delete h.top;
    return 0;
}
