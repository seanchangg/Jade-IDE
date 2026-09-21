// jade_latency.h — header-only latency timer, log-bucket histogram, and CSV
// output for microbenchmarks. C++17, no dependencies, nothing allocates on
// the hot path.
//
//   #include "jade_latency.h"
//
//   // 1. Name a series and time a block into it.
//   for (auto& order : replay) {
//       JADE_TIME("apply");                       // this scope, into series "apply"
//       book.apply(order);
//   }
//
//   // 2. Or time a callable and keep its result.
//   auto px = jade::measure("best_bid", [&] { return book.best_bid(); });
//
//   // 3. Or run a benchmark loop: warm, cold, or batched.
//   jade::bench("apply", 100000, [&] { book.apply(next()); });
//   jade::bench("apply cold", 2000, [&] { book.apply(next()); }, jade::Cold{});
//   jade::bench("apply x32", 100000, [&] { book.apply(next()); }, jade::BatchOf{32});
//
//   // 4. Report everything that was named.
//   jade::report(stderr);                          // one summary line per series
//   jade::write_csv("latency.csv");                // buckets of every series
//   jade::write_summary_csv("summary.csv");        // one row per series
//
// The named series live in a registry; the macro looks its series up once
// per call site, so the hot path pays one static load. A `jade::Histogram`
// can also be held directly and timed with `jade::Scoped`, `jade::Batch`,
// or `jade::Timer`. Every timer takes a Clock type parameter, default
// `jade::SteadyClock`; supply your own with a static `uint64_t now_ns()`
// to time with a cycle counter or a simulated clock.
//
// Clock: mach_absolute_time on macOS (24 MHz on Apple silicon, so one tick
// is 41.7 ns), CLOCK_MONOTONIC elsewhere. An operation under 100 ns lands in
// two or three buckets from quantization alone; time a batch of them with
// jade::Batch and the histogram records the per-operation share.
//
// Buckets: 8 per octave (power of two), from 1 ns to about 18 minutes.
// Percentiles come from the buckets, so they carry the bucket's width as
// error: 12.5 percent at most.

#pragma once

#if defined(__cplusplus) && __cplusplus < 201703L
#error "jade_latency.h needs C++17: compile with -std=c++17 or newer"
#endif

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cmath>
#include <map>
#include <string>
#include <type_traits>
#include <utility>

#if defined(__APPLE__)
#include <mach/mach_time.h>
#else
#include <time.h>
#endif

namespace jade {

// ── clock ───────────────────────────────────────────────────────────────────

/// Monotonic nanoseconds. Cheap: one instruction plus a multiply on macOS.
inline uint64_t now_ns() {
#if defined(__APPLE__)
    static const mach_timebase_info_data_t tb = [] {
        mach_timebase_info_data_t t;
        mach_timebase_info(&t);
        return t;
    }();
    return mach_absolute_time() * tb.numer / tb.denom;
#else
    timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
#endif
}

/// The clock's tick in nanoseconds: 41.7 on Apple silicon. Samples below a
/// few ticks are quantized.
inline double clock_tick_ns() {
#if defined(__APPLE__)
    mach_timebase_info_data_t t;
    mach_timebase_info(&t);
    return (double)t.numer / (double)t.denom;
#else
    timespec r;
    clock_getres(CLOCK_MONOTONIC, &r);
    return (double)r.tv_nsec;
#endif
}

/// The default clock for every timer. A custom clock is any type with a
/// static `uint64_t now_ns()`.
struct SteadyClock {
    static uint64_t now_ns() { return jade::now_ns(); }
};

// ── histogram ───────────────────────────────────────────────────────────────

struct Histogram {
    static constexpr int FRAC_BITS = 3;                 // 8 sub-buckets per octave
    static constexpr int SUB = 1 << FRAC_BITS;
    static constexpr int MAX_LOG2 = 40;                 // ~18 minutes in ns
    static constexpr int BUCKETS = (MAX_LOG2 - FRAC_BITS + 2) * SUB;

    uint64_t count[BUCKETS];
    uint64_t samples = 0;
    uint64_t sum_ns = 0;
    uint64_t min_ns = ~0ull;
    uint64_t max_ns = 0;

    Histogram() { std::memset(count, 0, sizeof(count)); }

    void reset() { *this = Histogram(); }

    /// Bucket index of a value. Values under SUB get their own bucket each;
    /// above that, the top FRAC_BITS bits after the leading one select the
    /// sub-bucket within the octave.
    static int bucket(uint64_t ns) {
        if (ns < (uint64_t)SUB) return (int)ns;
        int lg = 63 - __builtin_clzll(ns);
        int sub = (int)((ns >> (lg - FRAC_BITS)) & (SUB - 1));
        int b = (lg - FRAC_BITS + 1) * SUB + sub;
        return b < BUCKETS ? b : BUCKETS - 1;
    }

    /// Smallest value that lands in bucket b.
    static uint64_t lower_ns(int b) {
        if (b < SUB) return (uint64_t)b;
        int oct = b / SUB - 1 + FRAC_BITS;
        int sub = b % SUB;
        return ((uint64_t)(SUB | sub)) << (oct - FRAC_BITS);
    }

    /// Largest value that lands in bucket b.
    static uint64_t upper_ns(int b) {
        return b + 1 < BUCKETS ? lower_ns(b + 1) - 1 : ~0ull;
    }

    void add(uint64_t ns) {
        ++count[bucket(ns)];
        ++samples;
        sum_ns += ns;
        if (ns < min_ns) min_ns = ns;
        if (ns > max_ns) max_ns = ns;
    }

    /// Value at or below which fraction p of the samples fall, as the upper
    /// bound of the bucket that crosses p. p in [0, 1].
    uint64_t percentile(double p) const {
        if (samples == 0) return 0;
        if (p >= 1.0) return max_ns;
        uint64_t need = (uint64_t)std::ceil(p * (double)samples);
        if (need == 0) need = 1;
        uint64_t seen = 0;
        for (int b = 0; b < BUCKETS; ++b) {
            seen += count[b];
            if (seen >= need) {
                uint64_t hi = upper_ns(b);
                return hi < max_ns ? hi : max_ns;
            }
        }
        return max_ns;
    }

    double mean_ns() const { return samples ? (double)sum_ns / (double)samples : 0.0; }

    /// Fold another histogram in (per-thread histograms, merged at the end).
    void merge(const Histogram& o) {
        for (int b = 0; b < BUCKETS; ++b) count[b] += o.count[b];
        samples += o.samples;
        sum_ns += o.sum_ns;
        if (o.min_ns < min_ns) min_ns = o.min_ns;
        if (o.max_ns > max_ns) max_ns = o.max_ns;
    }

    // ── output ──────────────────────────────────────────────────────────────

    /// One line: samples, mean, p50, p90, p99, p99.9, max.
    void print_summary(FILE* f, const char* name) const {
        std::fprintf(f, "%s: n=%llu mean=%.1fns p50=%lluns p90=%lluns p99=%lluns p99.9=%lluns max=%lluns\n",
                     name, (unsigned long long)samples, mean_ns(), (unsigned long long)percentile(0.50),
                     (unsigned long long)percentile(0.90), (unsigned long long)percentile(0.99),
                     (unsigned long long)percentile(0.999), (unsigned long long)max_ns);
    }

    /// Non-empty buckets as `name,lower_ns,upper_ns,count`. Writes a header
    /// when the file is new, appends otherwise, so several runs or several
    /// names share one file.
    bool write_csv(const char* path, const char* name) const {
        bool fresh = !exists(path);
        FILE* f = std::fopen(path, "a");
        if (!f) return false;
        if (fresh) std::fputs("name,lower_ns,upper_ns,count\n", f);
        for (int b = 0; b < BUCKETS; ++b) {
            if (!count[b]) continue;
            std::fprintf(f, "%s,%llu,%llu,%llu\n", name, (unsigned long long)lower_ns(b),
                         (unsigned long long)upper_ns(b), (unsigned long long)count[b]);
        }
        std::fclose(f);
        return true;
    }

    /// One row: `name,samples,mean_ns,p50_ns,p90_ns,p99_ns,p999_ns,max_ns`.
    bool write_summary_csv(const char* path, const char* name) const {
        bool fresh = !exists(path);
        FILE* f = std::fopen(path, "a");
        if (!f) return false;
        if (fresh) std::fputs("name,samples,mean_ns,p50_ns,p90_ns,p99_ns,p999_ns,max_ns\n", f);
        std::fprintf(f, "%s,%llu,%.1f,%llu,%llu,%llu,%llu,%llu\n", name, (unsigned long long)samples, mean_ns(),
                     (unsigned long long)percentile(0.50), (unsigned long long)percentile(0.90),
                     (unsigned long long)percentile(0.99), (unsigned long long)percentile(0.999),
                     (unsigned long long)max_ns);
        std::fclose(f);
        return true;
    }

private:
    static bool exists(const char* path) {
        FILE* f = std::fopen(path, "r");
        if (!f) return false;
        std::fclose(f);
        return true;
    }
};

// ── timers ──────────────────────────────────────────────────────────────────

/// Times its scope into a histogram.
template <class Clock = SteadyClock>
struct BasicScoped {
    Histogram& h;
    uint64_t t0;
    explicit BasicScoped(Histogram& hist) : h(hist), t0(Clock::now_ns()) {}
    ~BasicScoped() { h.add(Clock::now_ns() - t0); }
    BasicScoped(const BasicScoped&) = delete;
    BasicScoped& operator=(const BasicScoped&) = delete;
};
using Scoped = BasicScoped<>;

/// Times its scope, divides by n, and records the per-operation share.
/// For operations faster than a few clock ticks.
template <class Clock = SteadyClock>
struct BasicBatch {
    Histogram& h;
    uint64_t n;
    uint64_t t0;
    BasicBatch(Histogram& hist, uint64_t ops) : h(hist), n(ops ? ops : 1), t0(Clock::now_ns()) {}
    ~BasicBatch() {
        uint64_t per = (Clock::now_ns() - t0) / n;
        for (uint64_t i = 0; i < n; ++i) h.add(per);
    }
    BasicBatch(const BasicBatch&) = delete;
    BasicBatch& operator=(const BasicBatch&) = delete;
};
using Batch = BasicBatch<>;

/// Manual start/stop for paths that are not one scope.
template <class Clock = SteadyClock>
struct BasicTimer {
    uint64_t t0 = 0;
    void start() { t0 = Clock::now_ns(); }
    uint64_t stop_ns() const { return Clock::now_ns() - t0; }
    void stop_into(Histogram& h) const { h.add(stop_ns()); }
};
using Timer = BasicTimer<>;

// ── callables ───────────────────────────────────────────────────────────────

/// Time one call of `f` into `h` and return what it returned. Works for
/// void callables too.
template <class Clock = SteadyClock, class F>
decltype(auto) timed(Histogram& h, F&& f) {
    if constexpr (std::is_void_v<std::invoke_result_t<F&>>) {
        BasicScoped<Clock> t(h);
        std::forward<F>(f)();
    } else {
        BasicScoped<Clock> t(h);
        return std::forward<F>(f)();
    }
}

/// Benchmark modes for `bench`: each sample warm (default), each sample
/// after a cache eviction, or n calls per sample for fast operations.
struct Warm {};
struct Cold {
    size_t evict_bytes = 64u << 20;
};
struct BatchOf {
    uint64_t n;
};

inline void evict_cache(size_t bytes);

/// Call `f` `iters` times, timing each call into `h`.
template <class Clock = SteadyClock, class F>
void bench(Histogram& h, uint64_t iters, F&& f, Warm = {}) {
    for (uint64_t i = 0; i < iters; ++i) {
        BasicScoped<Clock> t(h);
        f();
    }
}

/// Call `f` `iters` times, evicting the caches before each timed call.
template <class Clock = SteadyClock, class F>
void bench(Histogram& h, uint64_t iters, F&& f, Cold cold) {
    for (uint64_t i = 0; i < iters; ++i) {
        evict_cache(cold.evict_bytes);
        BasicScoped<Clock> t(h);
        f();
    }
}

/// Call `f` `iters` times in groups of `batch.n`, recording the per-call
/// share of each group. `iters` rounds down to a whole number of groups.
template <class Clock = SteadyClock, class F>
void bench(Histogram& h, uint64_t iters, F&& f, BatchOf batch) {
    uint64_t n = batch.n ? batch.n : 1;
    for (uint64_t i = 0; i + n <= iters; i += n) {
        BasicBatch<Clock> t(h, n);
        for (uint64_t k = 0; k < n; ++k) f();
    }
}

// ── named series ────────────────────────────────────────────────────────────

/// The registry of named histograms. One per process; the map is touched
/// only on first use of a name and at report time.
inline std::map<std::string, Histogram>& registry() {
    static std::map<std::string, Histogram> r;
    return r;
}

/// The histogram for a name, created on first use.
inline Histogram& series(const char* name) { return registry()[name]; }

/// Time one call of `f` into the named series and return its result.
template <class Clock = SteadyClock, class F>
decltype(auto) measure(const char* name, F&& f) {
    return timed<Clock>(series(name), std::forward<F>(f));
}

/// `bench` into a named series. `mode` is `Warm{}`, `Cold{}`, or `BatchOf{n}`.
template <class Clock = SteadyClock, class F, class Mode = Warm>
void bench(const char* name, uint64_t iters, F&& f, Mode mode = {}) {
    bench<Clock>(series(name), iters, std::forward<F>(f), mode);
}

/// One summary line per named series.
inline void report(FILE* f = stderr) {
    for (auto& [name, h] : registry()) h.print_summary(f, name.c_str());
}

/// Buckets of every named series into one CSV.
inline bool write_csv(const char* path) {
    bool ok = true;
    for (auto& [name, h] : registry()) ok = h.write_csv(path, name.c_str()) && ok;
    return ok;
}

/// One summary row per named series.
inline bool write_summary_csv(const char* path) {
    bool ok = true;
    for (auto& [name, h] : registry()) ok = h.write_summary_csv(path, name.c_str()) && ok;
    return ok;
}

/// Drop every named series.
inline void reset_all() { registry().clear(); }

/// Time the enclosing scope into the named series. The lookup runs once
/// per call site.
#define JADE_TIME(name)                                              \
    static ::jade::Histogram& JADE_CAT_(jade_series_, __LINE__) = ::jade::series(name); \
    ::jade::Scoped JADE_CAT_(jade_scope_, __LINE__)(JADE_CAT_(jade_series_, __LINE__))
#define JADE_CAT2_(a, b) a##b
#define JADE_CAT_(a, b) JADE_CAT2_(a, b)

// ── cold cache ──────────────────────────────────────────────────────────────

/// Walk a buffer larger than the last level cache so the next sample runs
/// cold. Call between samples for the cold histogram. 64 MB covers every
/// Apple silicon last level cache.
inline void evict_cache(size_t bytes) {
    static char* buf = nullptr;
    static size_t have = 0;
    if (have < bytes) {
        delete[] buf;
        buf = new char[bytes];
        have = bytes;
        std::memset(buf, 1, bytes);
    }
    volatile char sink = 0;
    for (size_t i = 0; i < bytes; i += 64) sink = sink + buf[i];
    (void)sink;
}

inline void evict_cache() { evict_cache(64u << 20); }

}  // namespace jade
