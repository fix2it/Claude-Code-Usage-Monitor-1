use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;

use windows::core::{Interface, BOOL};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use crate::theme_engine::RenderedTheme;

thread_local! {
    // Desktop surfaces are created, rendered, and destroyed on the window
    // thread. Keeping their COM objects thread-local reflects that ownership
    // and avoids claiming the interfaces are Send/Sync.
    static PRESENTERS: RefCell<HashMap<isize, DesktopPresenter>> = RefCell::new(HashMap::new());
}

struct DesktopPresenter {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    composition: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    swap_chain: IDXGISwapChain1,
    width: u32,
    height: u32,
}

impl DesktopPresenter {
    unsafe fn create(hwnd: HWND, width: u32, height: u32) -> windows::core::Result<Self> {
        let mut device = None;
        let mut context = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )?;
        let device = device.expect("D3D11CreateDevice succeeded without a device");
        let context = context.expect("D3D11CreateDevice succeeded without a context");

        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter = dxgi_device.GetAdapter()?;
        let factory: IDXGIFactory2 = adapter.GetParent()?;
        let description = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: BOOL(0),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let swap_chain = factory.CreateSwapChainForComposition(&device, &description, None)?;

        let composition: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
        let target = composition.CreateTargetForHwnd(hwnd, true)?;
        let visual = composition.CreateVisual()?;
        visual.SetContent(&swap_chain)?;
        target.SetRoot(&visual)?;
        composition.Commit()?;

        Ok(Self {
            _device: device,
            context,
            composition,
            _target: target,
            _visual: visual,
            swap_chain,
            width,
            height,
        })
    }

    /// A mode transition animates the surface width on every 16 ms tick.
    /// Recreating the swap chain for each frame stalls D3D and flickers, so
    /// resize the existing one in place instead. The visual keeps the same
    /// swap-chain object; `present()` reacquires buffer 0 after every resize.
    unsafe fn resize(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
        self.swap_chain.ResizeBuffers(
            2,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_SWAP_CHAIN_FLAG(0),
        )?;
        self.width = width;
        self.height = height;
        Ok(())
    }

    unsafe fn present(&self, rendered: &RenderedTheme) -> windows::core::Result<()> {
        let back_buffer: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;
        self.context.UpdateSubresource(
            &back_buffer,
            0,
            None,
            rendered.pixels.as_ptr().cast::<c_void>(),
            rendered.width.saturating_mul(4),
            0,
        );
        self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
        self.composition.Commit()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresenterAction {
    Create,
    Resize,
    Keep,
}

/// Pure decision for one frame. A width change during a 200 ms mode
/// transition must resize, never recreate: recreating the presenter on every
/// tick was the desktop-nest flicker this stage removes. Keeping it pure makes
/// the "zero recreations during a transition" invariant unit-testable without
/// a D3D device.
fn presenter_action(current: Option<(u32, u32)>, next: (u32, u32)) -> PresenterAction {
    match current {
        None => PresenterAction::Create,
        Some(size) if size == next => PresenterAction::Keep,
        Some(_) => PresenterAction::Resize,
    }
}

pub fn present(hwnd: HWND, rendered: &RenderedTheme) -> Result<(), String> {
    if rendered.width == 0 || rendered.height == 0 {
        return Err("desktop surface has no pixels".into());
    }
    PRESENTERS.with(|presenters| {
        let mut presenters = presenters.borrow_mut();
        let key = hwnd.0 as isize;
        let next = (rendered.width, rendered.height);
        match presenter_action(
            presenters
                .get(&key)
                .map(|presenter| (presenter.width, presenter.height)),
            next,
        ) {
            PresenterAction::Resize => {
                let presenter = presenters
                    .get_mut(&key)
                    .expect("an existing presenter is required for a resize");
                unsafe { presenter.resize(rendered.width, rendered.height) }
                    .map_err(|error| format!("DirectComposition resize failed: {error}"))?;
            }
            PresenterAction::Create => {
                let presenter =
                    unsafe { DesktopPresenter::create(hwnd, rendered.width, rendered.height) }
                        .map_err(|error| format!("DirectComposition setup failed: {error}"))?;
                presenters.insert(key, presenter);
            }
            PresenterAction::Keep => {}
        }
        unsafe { presenters[&key].present(rendered) }
            .map_err(|error| format!("DirectComposition present failed: {error}"))
    })
}

pub fn remove(hwnd: HWND) {
    PRESENTERS.with(|presenters| {
        presenters.borrow_mut().remove(&(hwnd.0 as isize));
    });
}

pub fn clear() {
    PRESENTERS.with(|presenters| presenters.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_transition_resizes_instead_of_recreating_the_presenter() {
        assert_eq!(presenter_action(None, (200, 46)), PresenterAction::Create);
        assert_eq!(
            presenter_action(Some((200, 46)), (200, 46)),
            PresenterAction::Keep
        );

        // ~12 width ticks across one 200 ms segment: not one may recreate.
        let mut size = (200, 46);
        for width in (80..=200).step_by(10) {
            let action = presenter_action(Some(size), (width, 46));
            assert_ne!(
                action,
                PresenterAction::Create,
                "width {size:?} -> {width} recreated the presenter"
            );
            assert_eq!(
                action,
                if width == size.0 {
                    PresenterAction::Keep
                } else {
                    PresenterAction::Resize
                }
            );
            size = (width, 46);
        }
    }

    #[test]
    fn a_height_change_also_resizes_in_place() {
        assert_eq!(
            presenter_action(Some((200, 46)), (200, 60)),
            PresenterAction::Resize
        );
    }
}
