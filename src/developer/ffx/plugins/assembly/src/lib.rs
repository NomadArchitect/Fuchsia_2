// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {anyhow::Result, ffx_assembly_args::*, ffx_core::ffx_plugin};

mod operations;
pub mod vfs;

#[ffx_plugin("assembly_enabled")]
pub async fn assembly(cmd: AssemblyCommand) -> Result<()> {
    // Dispatch to the correct operation based on the command.
    match cmd.op_class {
        OperationClass::VBMeta(vbmeta_op) => match vbmeta_op.operation {
            VBMetaOperation::Sign(args) => operations::vbmeta::sign(args),
        },
        // placeholder
        OperationClass::Image(args) => operations::image::assemble(args),
    }
}
