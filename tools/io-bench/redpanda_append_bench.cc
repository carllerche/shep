// Real-file companion to shep-append/examples/io_compare.rs.
// This harness links Redpanda's production segment_appender without changing
// it.
#include "base/seastarx.h"
#include "config/configuration.h"
#include "resource_mgmt/memory_groups.h"
#include "storage/chunk_cache.h"
#include "storage/segment_appender.h"
#include "storage/storage_resources.h"

#include <seastar/core/app-template.hh>
#include <seastar/core/coroutine.hh>
#include <seastar/core/file.hh>
#include <seastar/core/reactor.hh>
#include <seastar/core/shared_future.hh>
#include <seastar/core/temporary_buffer.hh>
#include <seastar/core/when_all.hh>

#include <boost/program_options.hpp>
#include <fmt/format.h>
#include <sys/resource.h>

#include <algorithm>
#include <chrono>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

namespace {
using clock_type = std::chrono::steady_clock;
constexpr size_t alignment = 4096;
constexpr size_t read_size = 64 * 1024;
constexpr size_t read_blocks = 512;

// Common workload geometry, shared with the Rust companion.
struct options {
    // Fresh directory containing this process's benchmark files.
    std::string directory;
    // Durable, streaming, or durable writes with background readers.
    std::string mode{"durable"};
    // Concurrent files, each driven by one sequential producer.
    size_t groups{8};
    // Measured appends per file, excluding warmup.
    size_t iterations{1000};
    // Untimed appends per file, synchronized before measurement.
    size_t warmup{64};
    // Logical payload bytes per append.
    size_t bytes{4096};
    // Appender chunk size in bytes, matched to the Rust companion.
    size_t chunk{64 * 1024};
    // Shared chunk-cache budget in bytes, distinct from Seastar's arena.
    size_t pool{30 * 1024 * 1024};
    // Byte increment reserved ahead of the logical file end.
    size_t preallocate{2 * 1024 * 1024};
    // Concurrent direct range readers in mixed mode.
    size_t readers{4};
};
// Process-wide resource snapshot, including all Seastar helper threads.
struct usage {
    // getrusage totals; before/after differences exclude setup and
    // verification.
    rusage value{};
    // Snapshot process-wide CPU usage, matching the Rust getrusage boundary.
    usage() {
        if (getrusage(RUSAGE_SELF, &value) != 0) {
            throw std::runtime_error("getrusage failed");
        }
    }
    // Count user and kernel time across every process thread.
    double cpu() const {
        return double(value.ru_utime.tv_sec + value.ru_stime.tv_sec)
               + double(value.ru_utime.tv_usec + value.ru_stime.tv_usec) / 1e6;
    }
};
// Completed physical I/O totals; snapshot deltas exclude setup work.
struct counters {
    // Successful write calls, physical bytes, syncs, and space reservations.
    uint64_t writes{0}, bytes{0}, fsyncs{0}, allocations{0};
};
uint64_t elapsed_ns(clock_type::time_point start) {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
             clock_type::now() - start)
      .count();
}
// Use the same integer-rank definition as the Rust companion and Python
// auditor.
uint64_t percentile(std::vector<uint64_t>& values, size_t p) {
    if (values.empty()) {
        return 0;
    }
    std::sort(values.begin(), values.end());
    return values[(values.size() - 1) * p / 1000];
}
// Aggregate native appender counters; setup deltas are excluded later.
counters
snapshot(const std::vector<storage::segment_appender::stats_ptr>& stats) {
    counters result;
    for (const auto& s : stats) {
        result.writes += s->writes_completed;
        result.bytes += s->bytes_written;
        result.fsyncs += s->fsyncs;
        result.allocations += s->fallocations;
    }
    return result;
}
// Verify EOF and every logical byte after close, outside the timed window.
void verify(const std::filesystem::path& path, uint64_t expected) {
    if (std::filesystem::file_size(path) != expected) {
        throw std::runtime_error("incorrect final file size");
    }
    std::ifstream input(path, std::ios::binary);
    std::vector<char> buffer(1024 * 1024);
    uint64_t checked = 0;
    while (input) {
        input.read(buffer.data(), buffer.size());
        const auto n = input.gcount();
        if (!std::all_of(buffer.begin(), buffer.begin() + n, [](char b) {
                return b == 'Z';
            })) {
            throw std::runtime_error("file content verification failed");
        }
        checked += n;
    }
    if (checked != expected) {
        throw std::runtime_error("short verification read");
    }
}
// Preserve unsorted samples so the runner can independently validate
// percentiles.
void samples(
  const std::filesystem::path& path,
  const std::vector<uint64_t>& writes,
  const std::vector<uint64_t>& reads) {
    std::ofstream output(path);
    output << "operation,latency_ns\n";
    for (auto ns : writes) {
        output << "append," << ns << '\n';
    }
    for (auto ns : reads) {
        output << "read," << ns << '\n';
    }
    if (!output) {
        throw std::runtime_error("cannot save operation samples");
    }
}
// One closed-loop producer per file; streaming still awaits its final sync.
ss::future<std::vector<uint64_t>> write_group(
  storage::segment_appender& appender,
  const options& opts,
  const std::vector<char>& payload,
  ss::future<> start) {
    std::vector<uint64_t> result;
    result.reserve(opts.iterations);
    co_await std::move(start);
    for (size_t i = 0; i < opts.iterations; ++i) {
        const auto before = clock_type::now();
        co_await appender.append(payload.data(), payload.size());
        if (opts.mode != "stream") {
            co_await appender.flush();
        }
        result.push_back(elapsed_ns(before));
    }
    // Stream throughput includes synchronization; close/trimming stays outside
    // timing.
    co_await appender.flush();
    co_return result;
}
// Reuse one aligned buffer and the common LCG range sequence per reader.
ss::future<std::vector<uint64_t>>
read_loop(ss::file file, size_t index, bool& stop, ss::future<> start) {
    auto buffer = ss::temporary_buffer<char>::aligned(alignment, read_size);
    std::vector<uint64_t> result;
    co_await std::move(start);
    while (!stop) {
        index = (index * 73 + 17) % read_blocks;
        const auto before = clock_type::now();
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
        const auto n = co_await file.dma_read(
          index * read_size, buffer.get_write(), read_size);
#pragma clang diagnostic pop
        if (
          n != read_size || buffer.get()[0] != 'Z'
          || buffer.get()[read_size - 1] != 'Z') {
            throw std::runtime_error("incorrect mixed read");
        }
        result.push_back(elapsed_ns(before));
    }
    co_return result;
}
// Configure the production appender, warm it, then start all workers together.
ss::future<int> run(options opts) {
    if (
      ss::this_smp_shard_count() != 1 || opts.groups == 0
      || opts.iterations == 0 || opts.bytes == 0 || opts.chunk < alignment
      || opts.chunk % alignment != 0 || opts.pool < opts.chunk
      || opts.pool % 15 != 0 || opts.pool % opts.chunk != 0
      || opts.preallocate == 0 || opts.preallocate % alignment != 0
      || (opts.mode != "durable" && opts.mode != "stream" && opts.mode != "mixed")) {
        throw std::runtime_error(
          "invalid benchmark geometry (pool must divide by 15 and chunk size)");
    }
    auto& cfg = config::shard_local_cfg();
    cfg.append_chunk_size.set_value(opts.chunk);
    cfg.segment_fallocation_step.set_value(opts.preallocate);
    cfg.segment_appender_flush_timeout_ms.set_value(
      std::chrono::milliseconds(1000));
    cfg.disable_metrics.set_value(true);
    // Production memory groups allocate 15 of 87 shares to chunks when optional
    // subsystems and reservations are off. Assert the resulting budget rather
    // than silently relying on this ratio across Redpanda revisions.
    memory_groups_holder().emplace(
      opts.pool / 15 * 87,
      compaction_memory_reservation{},
      cloud_topics_compaction_memory_reservation{},
      cloud_topics_reconciler_memory_reservation{},
      data_transforms_memory_reservation{},
      false,
      false,
      false,
      partitions_memory_reservation{0});
    if (memory_groups().chunk_cache_max_memory() != opts.pool) {
        throw std::runtime_error("Redpanda memory-share formula changed");
    }
    storage::storage_resources resources;
    co_await resources.start();
    std::vector<std::unique_ptr<storage::segment_appender>> appenders;
    std::vector<storage::segment_appender::stats_ptr> stats;
    std::vector<std::filesystem::path> paths;
    std::vector<char> payload(opts.bytes, 'Z');
    for (size_t i = 0; i < opts.groups; ++i) {
        auto path = std::filesystem::path(opts.directory)
                    / ("log-" + std::to_string(i));
        auto file = co_await ss::open_file_dma(
          path.string(),
          ss::open_flags::rw | ss::open_flags::create
            | ss::open_flags::exclusive,
          ss::file_open_options{
            .extent_allocation_size_hint = 0, .sloppy_size = false});
        stats.push_back(ss::make_lw_shared<storage::segment_appender::stats>());
        appenders.push_back(
          std::make_unique<storage::segment_appender>(
            std::move(file),
            storage::segment_appender::options(
              std::nullopt, resources, stats.back())));
        paths.push_back(path);
        for (size_t j = 0; j < opts.warmup; ++j) {
            co_await appenders.back()->append(payload.data(), payload.size());
            if (opts.mode != "stream") {
                co_await appenders.back()->flush();
            }
        }
        co_await appenders.back()->flush();
    }
    ss::file reader_file;
    if (opts.mode == "mixed") {
        reader_file = co_await ss::open_file_dma(
          opts.directory + "/read-data",
          ss::open_flags::rw | ss::open_flags::create
            | ss::open_flags::exclusive);
        auto block = ss::temporary_buffer<char>::aligned(alignment, read_size);
        std::fill_n(block.get_write(), read_size, 'Z');
        for (size_t i = 0; i < read_blocks; ++i) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
            if (
              co_await reader_file.dma_write(
                i * read_size, block.get(), read_size)
              != read_size) {
                throw std::runtime_error("short reader-fixture write");
            }
#pragma clang diagnostic pop
        }
        co_await reader_file.flush();
    }
    co_await ss::sync_directory(opts.directory);
    ss::shared_promise<> start;
    bool stop = false;
    std::vector<ss::future<std::vector<uint64_t>>> writers, readers;
    for (auto& appender : appenders) {
        writers.push_back(
          write_group(*appender, opts, payload, start.get_shared_future()));
    }
    if (opts.mode == "mixed") {
        for (size_t i = 0; i < opts.readers; ++i) {
            readers.push_back(
              read_loop(reader_file, i, stop, start.get_shared_future()));
        }
    }
    const auto before = snapshot(stats);
    const usage cpu_before;
    const auto began = clock_type::now();
    start.set_value();
    std::vector<uint64_t> write_samples, read_samples;
    for (auto& writer : writers) {
        auto values = co_await std::move(writer);
        write_samples.insert(write_samples.end(), values.begin(), values.end());
    }
    const auto duration_ns = elapsed_ns(began);
    const usage cpu_after;
    stop = true;
    const auto after = snapshot(stats);
    for (auto& reader : readers) {
        auto values = co_await std::move(reader);
        read_samples.insert(read_samples.end(), values.begin(), values.end());
    }
    const auto expected_syncs = opts.mode == "stream"
                                  ? opts.groups
                                  : opts.groups * opts.iterations;
    if (after.fsyncs - before.fsyncs != expected_syncs) {
        throw std::runtime_error("unexpected measured synchronization count");
    }
    // Verification, samples, close and cleanup are all outside the measured
    // window.
    for (auto& appender : appenders) {
        co_await appender->close();
    }
    if (opts.mode == "mixed") {
        co_await reader_file.close();
    }
    for (const auto& path : paths) {
        verify(path, (opts.warmup + opts.iterations) * opts.bytes);
    }
    samples(
      std::filesystem::path(opts.directory) / "latencies.csv",
      write_samples,
      read_samples);
    fmt::print(
      "SHEP_IO_BENCH "
      "{{\"engine\":\"redpanda\",\"layer\":\"append-file\",\"backend\":\"{}\","
      "\"mode\":\"{}\","
      "\"groups\":{},\"iterations\":{},\"warmup\":{},\"payload_bytes\":{},"
      "\"chunk_bytes\":{},\"pool_bytes\":{},\"alignment_bytes\":{},"
      "\"preallocate_bytes\":{},"
      "\"wall_ns\":{},\"cpu_seconds\":{},\"voluntary_switches\":{},"
      "\"involuntary_switches\":{},"
      "\"write_calls\":{},\"write_bytes\":{},\"fsyncs\":{},\"allocations\":{},"
      "\"p50_ns\":{},\"p99_ns\":{},\"p999_ns\":{},\"reads\":{},\"read_p99_ns\":"
      "{},\"verified\":true}}\n",
      ss::engine().get_backend_name(),
      opts.mode,
      opts.groups,
      opts.iterations,
      opts.warmup,
      opts.bytes,
      opts.chunk,
      opts.pool,
      alignment,
      opts.preallocate,
      duration_ns,
      cpu_after.cpu() - cpu_before.cpu(),
      cpu_after.value.ru_nvcsw - cpu_before.value.ru_nvcsw,
      cpu_after.value.ru_nivcsw - cpu_before.value.ru_nivcsw,
      after.writes - before.writes,
      after.bytes - before.bytes,
      after.fsyncs - before.fsyncs,
      after.allocations - before.allocations,
      percentile(write_samples, 500),
      percentile(write_samples, 990),
      percentile(write_samples, 999),
      read_samples.size(),
      percentile(read_samples, 990));
    appenders.clear();
    co_await resources.stop();
    co_return 0;
}
} // namespace
// Keep common workload flags separate from Seastar reactor configuration.
int main(int argc, char** argv) {
    namespace po = boost::program_options;
    ss::app_template app;
    options opts;
    app.add_options()(
      "directory",
      po::value<std::string>(&opts.directory)->required(),
      "Fresh benchmark directory")(
      "mode",
      po::value<std::string>(&opts.mode)->default_value(opts.mode),
      "durable, stream, or mixed")(
      "groups",
      po::value<size_t>(&opts.groups)->default_value(opts.groups),
      "Concurrent append files")(
      "iterations",
      po::value<size_t>(&opts.iterations)->default_value(opts.iterations),
      "Appends per file")(
      "warmup",
      po::value<size_t>(&opts.warmup)->default_value(opts.warmup),
      "Unmeasured appends per file")(
      "payload",
      po::value<size_t>(&opts.bytes)->default_value(opts.bytes),
      "Bytes per append")(
      "chunk",
      po::value<size_t>(&opts.chunk)->default_value(opts.chunk),
      "Write chunk bytes")(
      "pool",
      po::value<size_t>(&opts.pool)->default_value(opts.pool),
      "Shared chunk budget bytes")(
      "preallocate",
      po::value<size_t>(&opts.preallocate)->default_value(opts.preallocate),
      "Preallocation step bytes")(
      "readers",
      po::value<size_t>(&opts.readers)->default_value(opts.readers),
      "Mixed-mode readers");
    return app.run(argc, argv, [&opts] { return run(opts); });
}
