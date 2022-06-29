
// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_NVME_CPP_COMMANDS_IDENTIFY_H_
#define SRC_DEVICES_BLOCK_DRIVERS_NVME_CPP_COMMANDS_IDENTIFY_H_

#include <hwreg/bitfields.h>

#include "src/devices/block/drivers/nvme-cpp/commands.h"

namespace nvme {

// NVM Express Base Specification 2.0, section 5.17, "Identify command"
class IdentifySubmission : public Submission {
 public:
  static constexpr uint8_t kOpcode = 0x06;
  IdentifySubmission() : Submission(kOpcode) {}
  enum IdentifyCns {
    kIdentifyNamespace = 0,
    kIdentifyController = 1,
    kActiveNamespaceList = 2,
    kNamespaceIdentification = 3,
    kNvmSetList = 4,
    kIoCommandSetIdentifyNamespace = 5,
    kIoCommandSetIdentifyController = 6,
    kIoCommandSetActiveNamespaceList = 7,
    kIoCommandSetNamespaceIdentification = 8,
  };

  DEF_SUBFIELD(dword10, 31, 16, controller_id);
  DEF_ENUM_SUBFIELD(dword10, IdentifyCns, 7, 0, structure);
};

// NVM Express Base Specification 2.0, section 5.17.2.1, Figure 276, "Power State Descriptor Data
// Structure"
struct PowerStateDescriptor {
  uint32_t data[8];
};

// NVM Express Base Specification 2.0, section 5.17.2.1, "Identify Controller data structure"
struct IdentifyController {
  uint16_t pci_vid;
  uint16_t pci_did;
  char serial_number[20];
  char model_number[40];
  char firmware_rev[8];
  uint8_t recommended_arbitration_burst;
  uint8_t oui[3];
  uint8_t cmic;
  uint8_t max_data_transfer;
  uint16_t controller_id;
  uint32_t version;
  uint32_t rtd3_resume_latency;
  uint32_t rtd3_entry_latency;
  uint32_t oaes;
  uint32_t ctratt;
  uint16_t rrls;
  uint8_t reserved0[9];
  uint8_t controller_type;
  uint8_t fru_guid[16];
  uint16_t crdt1;
  uint16_t crdt2;
  uint16_t crdt3;
  uint8_t reserved1[119];
  uint8_t nvmsr;
  uint8_t vwci;
  uint8_t mec;

  // 0x100
  uint16_t oacs;
  uint8_t acl;
  uint8_t aerl;
  uint8_t frmw;
  uint8_t lpa;
  uint8_t elpe;
  uint8_t npss;
  uint8_t avscc;
  uint8_t apsta;
  uint16_t wctemp;
  uint16_t cctemp;
  uint16_t mtfa;
  uint32_t hmpre;
  uint32_t hmmin;
  uint64_t tnvmcap[2];
  uint64_t unvmcap[2];
  uint32_t rpmb_support;
  uint16_t edstt;
  uint8_t dsto;
  uint8_t fwug;
  uint16_t kas;
  uint16_t hctma;
  uint16_t mntmt;
  uint16_t mxtmt;
  uint32_t sanicap;
  uint32_t hmminds;
  uint16_t hmmaxd;
  uint16_t nsetid_max;
  uint16_t endgid_max;
  uint8_t ana_tt;
  uint8_t ana_cap;
  uint32_t ana_grp_max;
  uint32_t n_ana_grp_id;
  uint32_t pels;
  uint16_t domain_id;
  uint8_t reserved2[10];
  uint64_t max_egcap[2];

  uint8_t reserved3[128];

  // 0x200
  uint8_t sqes;
  uint8_t cqes;
  uint16_t max_cmd;
  uint32_t num_namespaces;
  uint16_t oncs;
  uint16_t fuses;
  uint8_t fna;
  uint8_t vwc;
  uint16_t atomic_write_unit_normal;
  uint16_t atomic_write_unit_power_fail;
  uint8_t icsvscc;
  uint8_t nwpc;
  uint16_t acwu;
  uint16_t copy_formats_supported;
  uint32_t sgl_support;
  uint32_t max_allowed_namespaces;
  uint64_t max_dna[2];
  uint32_t max_cna;

  uint8_t reserved4[204];

  // 0x300
  char nvme_qualified_name[256];

  // 0x400, 0x500, 0x600
  uint8_t reserved5[768];

  // 0x700
  uint32_t io_cc_size;
  uint32_t io_rc_size;
  uint16_t icdoff;
  uint8_t fcatt;
  uint8_t msdbd;
  uint16_t ofcs;

  uint8_t reserved6[242];

  // 0x800
  PowerStateDescriptor power_states[32];

  // 0xc00
  uint8_t vendor_data[1024];

  DEF_SUBFIELD(sqes, 3, 0, sqes_min_log2);
  DEF_SUBFIELD(cqes, 3, 0, cqes_min_log2);

  size_t minimum_sq_entry_size() const { return 1 << sqes_min_log2(); }
  size_t minimum_cq_entry_size() const { return 1 << cqes_min_log2(); }
};
static_assert(sizeof(IdentifyController) == 0x1000);

}  // namespace nvme

#endif  // SRC_DEVICES_BLOCK_DRIVERS_NVME_CPP_COMMANDS_IDENTIFY_H_
