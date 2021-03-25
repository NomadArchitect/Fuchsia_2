// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_SYSTEM_MONITOR_BIN_HARVESTER_DOCKYARD_PROXY_GRPC_H_
#define SRC_DEVELOPER_SYSTEM_MONITOR_BIN_HARVESTER_DOCKYARD_PROXY_GRPC_H_

#include <optional>

#include "dockyard_proxy.h"
#include "fuchsia_clock.h"
#include "src/developer/system_monitor/lib/dockyard/dockyard.h"
#include "src/developer/system_monitor/lib/proto/dockyard.grpc.pb.h"

#include <grpc++/grpc++.h>

namespace harvester {

namespace internal {

// Utility functions for munging data.

void ExtractPathsFromSampleList(
    std::vector<const std::string*>* dockyard_strings, const SampleList& list);

void BuildSampleListById(SampleListById* by_id,
                         const std::vector<dockyard::DockyardId>& id_list,
                         const SampleList& sample_list);

dockyard_proto::LogBatch BuildLogBatch(
    const std::vector<const std::string>& batch, uint64_t monotonic_time,
    std::optional<zx_time_t> time);
}  // namespace internal

class DockyardProxyGrpc : public DockyardProxy {
 public:
  DockyardProxyGrpc(std::shared_ptr<grpc::Channel> channel,
                    std::unique_ptr<FuchsiaClock> clock)
      : stub_(dockyard_proto::Dockyard::NewStub(channel)),
        clock_(std::move(clock)) {}

  explicit DockyardProxyGrpc(
      std::unique_ptr<dockyard_proto::Dockyard::StubInterface> stub,
      std::unique_ptr<FuchsiaClock> clock)
      : stub_(std::move(stub)), clock_(std::move(clock)) {}

  // |DockyardProxy|.
  DockyardProxyStatus Init() override;

  // |DockyardProxy|.
  DockyardProxyStatus SendLogs(
      const std::vector<const std::string>& batch) override;

  // |DockyardProxy|.
  DockyardProxyStatus SendInspectJson(const std::string& dockyard_path,
                                      const std::string& json) override;

  // |DockyardProxy|.
  DockyardProxyStatus SendSample(const std::string& dockyard_path,
                                 uint64_t value) override;

  // |DockyardProxy|.
  DockyardProxyStatus SendSampleList(const SampleList& list) override;

  // |DockyardProxy|.
  DockyardProxyStatus SendStringSampleList(
      const StringSampleList& list) override;

  // |DockyardProxy|.
  DockyardProxyStatus SendSamples(
      const SampleList& int_samples,
      const StringSampleList& string_samples) override;

 private:
  // A local stub for the remote Dockyard instance.
  std::unique_ptr<dockyard_proto::Dockyard::StubInterface> stub_;
  std::unique_ptr<harvester::FuchsiaClock> clock_;

  // The dockyard_path_to_id_ may be accessed by multiple threads.
  std::mutex dockyard_path_to_id_mutex_;
  // Look up the ID of a Dockyard path.
  std::map<std::string, dockyard::DockyardId> dockyard_path_to_id_ = {};

  // Functions for interacting with Dockyard (via gRPC).

  // Actually send data to the Dockyard.
  // |time| is in nanoseconds.
  // See also: SendInspectJson().
  grpc::Status SendInspectJsonById(std::optional<zx_time_t> time,
                                   dockyard::DockyardId dockyard_id,
                                   const std::string& json);

  // Actually send a single sample to the Dockyard.
  // |time| is in nanoseconds.
  // See also: SendSample().
  grpc::Status SendSampleById(std::optional<zx_time_t> time,
                              dockyard::DockyardId dockyard_id, uint64_t value);

  // Actually send a list of samples with the same timestamp to the Dockyard.
  // |time| is in nanoseconds.
  // See also: SendSampleList().
  grpc::Status SendSampleListById(std::optional<zx_time_t> time,
                                  const SampleListById& list);

  // Get the ID from the local cache or from the remote Dockyard if it's not in
  // the cache.
  grpc::Status GetDockyardIdForPath(dockyard::DockyardId* dockyard_id,
                                    const std::string& dockyard_path);
  // As above, for a list of paths and IDs.
  grpc::Status GetDockyardIdsForPaths(
      std::vector<dockyard::DockyardId>* dockyard_id,
      const std::vector<const std::string*>& dockyard_path);

  grpc::Status SendUtcClockStarted();
};

}  // namespace harvester

#endif  // SRC_DEVELOPER_SYSTEM_MONITOR_BIN_HARVESTER_DOCKYARD_PROXY_GRPC_H_
