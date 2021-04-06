// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ui/scenic/cpp/commands.h>

#include <gtest/gtest.h>

#include "src/ui/lib/escher/flib/fence.h"
#include "src/ui/lib/escher/test/common/gtest_vulkan.h"
#include "src/ui/lib/escher/util/image_utils.h"
#include "src/ui/lib/escher/vk/image_layout_updater.h"
#include "src/ui/scenic/lib/gfx/engine/engine_renderer_visitor.h"
#include "src/ui/scenic/lib/gfx/resources/image_pipe.h"
#include "src/ui/scenic/lib/gfx/resources/image_pipe2.h"
#include "src/ui/scenic/lib/gfx/resources/material.h"
#include "src/ui/scenic/lib/gfx/tests/image_pipe_unittest_common.h"
#include "src/ui/scenic/lib/gfx/tests/mocks/util.h"
#include "src/ui/scenic/lib/gfx/tests/vk_session_handler_test.h"

namespace scenic_impl::gfx::test {

class ImagePipeRenderTest : public VkSessionHandlerTest {
 public:
  // Create a one-time EngineRendererVisitor and GpuUploader to visit the material node /
  // scene node to upload ImagePipe images.
  template <typename T>
  void Visit(T* t) {
    auto gpu_uploader = escher::BatchGpuUploader(escher_->GetWeakPtr(), 0);
    auto image_layout_updater = escher::ImageLayoutUpdater(escher_->GetWeakPtr());
    EngineRendererVisitor visitor(/* paper_renderer */ nullptr, &gpu_uploader,
                                  &image_layout_updater,
                                  /* hide_protected_memory */ false,
                                  /* replacement_material */ escher::MaterialPtr());
    visitor.Visit(t);
    image_layout_updater.Submit();
    gpu_uploader.Submit();
  }
};

// Check that ImagePipe attached Material is transparent until the first frame is presented.
VK_TEST_F(ImagePipeRenderTest, TransparentUntilFirstUpdate) {
  ResourceId next_id = 1;
  ImagePipePtr image_pipe = fxl::MakeRefCounted<ImagePipe>(
      session(), next_id++, image_pipe_updater(), shared_error_reporter());
  MaterialPtr pipe_material = fxl::MakeRefCounted<Material>(session(), session()->id(), next_id++);
  pipe_material->SetTexture(image_pipe);

  // Material is transparent before present.
  Visit(pipe_material.get());
  EXPECT_EQ(0u, pipe_material->alpha());

  // Present an image and
  constexpr uint32_t kImageId = 1;
  constexpr size_t kImageDim = 50;
  {
    auto checkerboard = CreateVmoWithCheckerboardPixels(kImageDim, kImageDim);
    auto image_info = CreateImageInfoForBgra8Image(kImageDim, kImageDim);
    image_pipe->AddImage(kImageId, std::move(image_info), CopyVmo(checkerboard->vmo()), 0,
                         GetVmoSize(checkerboard->vmo()), fuchsia::images::MemoryType::HOST_MEMORY);
  }
  image_pipe->PresentImage(kImageId, zx::time(1), {}, {}, /*callback=*/[](auto) {});
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));

  // Material is transparent before present.
  Visit(pipe_material.get());
  EXPECT_EQ(1u, pipe_material->alpha());
}

// Present two frames on the ImagePipe, making sure that image is
// updated only after Visit().
VK_TEST_F(ImagePipeRenderTest, ImageUpdatedOnlyAfterVisit) {
  ResourceId next_id = 1;
  ImagePipePtr image_pipe = fxl::MakeRefCounted<ImagePipe>(
      session(), next_id++, image_pipe_updater(), shared_error_reporter());
  MaterialPtr pipe_material = fxl::MakeRefCounted<Material>(session(), session()->id(), next_id++);
  pipe_material->SetTexture(image_pipe);

  constexpr uint32_t kImage1Id = 1;
  constexpr size_t kImage1Dim = 50;
  // Create a checkerboard image and copy it into a vmo.
  {
    auto checkerboard = CreateVmoWithCheckerboardPixels(kImage1Dim, kImage1Dim);
    auto image_info = CreateImageInfoForBgra8Image(kImage1Dim, kImage1Dim);

    // Add the image to the image pipe with ImagePipe.AddImage().
    image_pipe->AddImage(kImage1Id, std::move(image_info), CopyVmo(checkerboard->vmo()), 0,
                         GetVmoSize(checkerboard->vmo()), fuchsia::images::MemoryType::HOST_MEMORY);
  }

  constexpr uint32_t kImage2Id = 2;
  constexpr size_t kImage2Dim = 100;
  // Create a new Image with a gradient.
  {
    auto gradient = CreateVmoWithGradientPixels(kImage2Dim, kImage2Dim);
    auto image_info = CreateImageInfoForBgra8Image(kImage2Dim, kImage2Dim);

    // Add the image to the image pipe.
    image_pipe->AddImage(kImage2Id, std::move(image_info), CopyVmo(gradient->vmo()), 0,
                         GetVmoSize(gradient->vmo()), fuchsia::images::MemoryType::HOST_MEMORY);
  }

  // We present Image 2 at time (0) and Image 1 at time (1).
  // Only Image 1 should be updated and uploaded.
  image_pipe->PresentImage(kImage2Id, zx::time(0), {}, {}, /*callback=*/[](auto) {});
  image_pipe->PresentImage(kImage1Id, zx::time(1), {}, {}, /*callback=*/[](auto) {});

  // After ImagePipeUpdater updates the ImagePipe, the current_image() should be set
  // but Escher image is not created.
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));
  ASSERT_TRUE(image_pipe->current_image());
  ASSERT_FALSE(image_pipe->GetEscherImage());
  auto image1 = image_pipe->current_image();

  // Escher image is not created until EngineRendererVisitor visits the material.
  Visit(pipe_material.get());
  auto escher_image1 = image_pipe->GetEscherImage();
  ASSERT_TRUE(escher_image1);
  ASSERT_EQ(escher_image1->width(), kImage1Dim);

  // We present Image 1 (already rendered) at time (0) and Image 2 (not rendered yet)
  // at time (1). Only Image 2 should be updated and uploaded.
  image_pipe->PresentImage(kImage1Id, zx::time(0), {}, {}, /*callback=*/[](auto) {});
  image_pipe->PresentImage(kImage2Id, zx::time(1), {}, {}, /*callback=*/[](auto) {});

  // After ImagePipeUpdater updates the ImagePipe, the current_image() should be set
  // but Escher image is not created.
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));
  ASSERT_TRUE(image_pipe->current_image());
  ASSERT_NE(image_pipe->current_image(), image1);
  ASSERT_FALSE(image_pipe->GetEscherImage());

  // Escher image is not created until EngineRendererVisitor visits the material.
  Visit(pipe_material.get());
  auto escher_image2 = image_pipe->GetEscherImage();
  ASSERT_TRUE(escher_image2);
  ASSERT_NE(escher_image2, escher_image1);
  ASSERT_EQ(escher_image2->width(), kImage2Dim);
}

// Present two frames on the ImagePipe, making sure that acquire fence is being listened to and
// release fences are signalled.
VK_TEST_F(ImagePipeRenderTest, ImagePipePresentTwoFrames) {
  ResourceId next_id = 1;
  ImagePipePtr image_pipe = fxl::MakeRefCounted<ImagePipe>(
      session(), next_id++, image_pipe_updater(), shared_error_reporter());
  MaterialPtr pipe_material = fxl::MakeRefCounted<Material>(session(), session()->id(), next_id++);
  pipe_material->SetTexture(image_pipe);

  uint32_t image1_id = 1;
  // Create a checkerboard image and copy it into a vmo.
  {
    size_t image_dim = 100;
    auto checkerboard = CreateVmoWithCheckerboardPixels(image_dim, image_dim);
    auto image_info = CreateImageInfoForBgra8Image(image_dim, image_dim);

    // Add the image to the image pipe with ImagePipe.AddImage().
    image_pipe->AddImage(image1_id, std::move(image_info), CopyVmo(checkerboard->vmo()), 0,
                         GetVmoSize(checkerboard->vmo()), fuchsia::images::MemoryType::HOST_MEMORY);
  }

  // Make checkerboard the currently displayed image.
  zx::event acquire_fence1 = CreateEvent();
  zx::event release_fence1 = CreateEvent();

  image_pipe->PresentImage(image1_id, zx::time(0), CopyEventIntoFidlArray(acquire_fence1),
                           CopyEventIntoFidlArray(release_fence1), /*callback=*/[](auto) {});

  // Current presented image should be null, since we haven't signalled
  // acquire fence yet.
  ASSERT_FALSE(RunLoopFor(zx::sec(1)));
  Visit(pipe_material.get());
  ASSERT_FALSE(image_pipe->current_image());
  ASSERT_FALSE(image_pipe->GetEscherImage());
  ASSERT_FALSE(IsEventSignalled(acquire_fence1, ZX_EVENT_SIGNALED));
  ASSERT_FALSE(IsEventSignalled(release_fence1, ZX_EVENT_SIGNALED));

  // Signal on the acquire fence.
  acquire_fence1.signal(0u, escher::kFenceSignalled);

  // Run until image1 is presented, but not rendered due to lack of engine renderer visitor.
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));
  Visit(pipe_material.get());

  // Image should now be presented.
  escher::ImagePtr image1 = image_pipe->GetEscherImage();
  ASSERT_TRUE(image1);

  uint32_t image2_id = 2;
  // Create a new Image with a gradient.
  {
    size_t image_dim = 100;
    auto gradient = CreateVmoWithGradientPixels(image_dim, image_dim);
    auto image_info = CreateImageInfoForBgra8Image(image_dim, image_dim);

    // Add the image to the image pipe.
    image_pipe->AddImage(image2_id, std::move(image_info), CopyVmo(gradient->vmo()), 0,
                         GetVmoSize(gradient->vmo()), fuchsia::images::MemoryType::HOST_MEMORY);
  }

  // The first image should not have been released.
  ASSERT_FALSE(RunLoopFor(zx::sec(1)));
  Visit(pipe_material.get());
  ASSERT_FALSE(IsEventSignalled(release_fence1, ZX_EVENT_SIGNALED));

  // Make gradient the currently displayed image.
  zx::event acquire_fence2 = CreateEvent();
  image_pipe->PresentImage(image2_id, zx::time(0), CopyEventIntoFidlArray(acquire_fence2),
                           /*release_fences=*/{}, /*callback=*/[](auto) {});

  // Verify that the currently display image hasn't changed yet, since we haven't signalled the
  // acquire fence.
  ASSERT_FALSE(RunLoopUntilIdle());
  Visit(pipe_material.get());
  ASSERT_TRUE(image_pipe->GetEscherImage());
  ASSERT_EQ(image_pipe->GetEscherImage(), image1);
  ASSERT_FALSE(IsEventSignalled(release_fence1, ZX_EVENT_SIGNALED));

  // Signal on the acquire fence.
  acquire_fence2.signal(0u, escher::kFenceSignalled);

  // There should be a new image presented.
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));
  ASSERT_TRUE(IsEventSignalled(release_fence1, ZX_EVENT_SIGNALED));
  Visit(pipe_material.get());
  escher::ImagePtr image2 = image_pipe->GetEscherImage();
  ASSERT_TRUE(image2);
  ASSERT_NE(image1, image2);
}

// Present two frames on the ImagePipe, using presentation callback of the first one to signal for
// the second.
VK_TEST_F(ImagePipeRenderTest, ImagePipePresentTwoFramesWithSignalling) {
  ResourceId next_id = 1;
  ImagePipePtr image_pipe = fxl::MakeRefCounted<ImagePipe>(
      session(), next_id++, image_pipe_updater(), shared_error_reporter());
  MaterialPtr pipe_material = fxl::MakeRefCounted<Material>(session(), session()->id(), next_id++);
  pipe_material->SetTexture(image_pipe);

  uint32_t image1_id = 1;
  uint32_t image2_id = 2;
  for (uint32_t i = 1; i <= 2; ++i) {
    // Create a checkerboard image and copy it into a vmo.
    size_t image_dim = 100;
    auto checkerboard = CreateVmoWithCheckerboardPixels(image_dim, image_dim);
    auto image_info = CreateImageInfoForBgra8Image(image_dim, image_dim);
    // Add the image to the image pipe with AddImage().
    image_pipe->AddImage(i, std::move(image_info), CopyVmo(checkerboard->vmo()), 0,
                         GetVmoSize(checkerboard->vmo()), fuchsia::images::MemoryType::HOST_MEMORY);
  }

  zx::event acquire_fence1 = CreateEvent();
  zx::event acquire_fence2 = CreateEvent();
  image_pipe->PresentImage(image1_id, zx::time(0), CopyEventIntoFidlArray(acquire_fence1),
                           /*release_fences=*/{},
                           /*callback=*/[this, &acquire_fence2](auto) {
                             acquire_fence2.signal(0u, escher::kFenceSignalled);
                             QuitLoop();
                           });
  image_pipe->PresentImage(image2_id, zx::time(1), CopyEventIntoFidlArray(acquire_fence2),
                           /*release_fences=*/{},
                           /*callback=*/[](auto) {});

  // Current presented image should be null, since we haven't signalled acquire fence yet.
  ASSERT_FALSE(RunLoopFor(zx::sec(1)));
  Visit(pipe_material.get());
  ASSERT_FALSE(image_pipe->current_image());
  ASSERT_FALSE(image_pipe->GetEscherImage());

  // Signal on the acquire fence and run until image1 is presented.
  acquire_fence1.signal(0u, escher::kFenceSignalled);
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));
  Visit(pipe_material.get());
  escher::ImagePtr image1 = image_pipe->GetEscherImage();
  ASSERT_TRUE(image1);

  // Run until image2 is presented.
  ASSERT_TRUE(RunLoopFor(zx::sec(1)));
  Visit(pipe_material.get());
  escher::ImagePtr image2 = image_pipe->GetEscherImage();
  ASSERT_TRUE(image2);
  ASSERT_NE(image1, image2);
}

}  // namespace scenic_impl::gfx::test
