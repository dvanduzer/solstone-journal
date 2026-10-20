// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) 2026 sol pbc

import AppKit
import ObjectiveC
import SwiftUI

@MainActor
private final class JournalSetupWindowMemory {
    let styleMask: NSWindow.StyleMask
    var appliedInitialSetupSize: Bool = false

    init(window: NSWindow) {
        self.styleMask = window.styleMask
    }
}

@MainActor
private var journalSetupWindowMemoryKey: UInt8 = 0

@MainActor
enum JournalSetupWindowConfigurator {
    static func apply(setupShowing: Bool, to window: NSWindow) {
        if setupShowing {
            let memory: JournalSetupWindowMemory
            if let existing = objc_getAssociatedObject(window, &journalSetupWindowMemoryKey) as? JournalSetupWindowMemory {
                memory = existing
            } else {
                memory = JournalSetupWindowMemory(window: window)
                objc_setAssociatedObject(window, &journalSetupWindowMemoryKey, memory, .OBJC_ASSOCIATION_RETAIN_NONATOMIC)
            }

            window.title = JournalFirstRunCopy.setUpSolstone
            window.titlebarAppearsTransparent = true
            window.titleVisibility = .hidden
            window.styleMask.insert(.fullSizeContentView)
            window.contentMinSize = NSSize(width: 880, height: 580)

            if !memory.appliedInitialSetupSize {
                let currentContentRect = window.contentRect(forFrameRect: window.frame)
                if currentContentRect.width < 880 || currentContentRect.height < 580 {
                    window.setContentSize(NSSize(width: 880, height: 580))
                }
                memory.appliedInitialSetupSize = true
            }
        } else {
            window.title = "journal"
            if let memory = objc_getAssociatedObject(window, &journalSetupWindowMemoryKey) as? JournalSetupWindowMemory {
                // Undo only what setup added; never write a whole mask (full screen lives in it).
                if !memory.styleMask.contains(.fullSizeContentView) {
                    window.styleMask.remove(.fullSizeContentView)
                }
                // Home's own title bar: opaque, title shown. Not read back from the window when
                // setup first shows, because SwiftUI may already have hidden the title for the
                // setup window by then, and home would keep it hidden.
                window.titlebarAppearsTransparent = false
                window.titleVisibility = .visible
                memory.appliedInitialSetupSize = false
            }
            window.contentMinSize = NSSize(width: 720, height: 500)
        }
    }
}

struct JournalSetupWindowInstaller: NSViewRepresentable {
    let setupShowing: Bool

    func makeNSView(context: Context) -> JournalSetupWindowHostView {
        let view = JournalSetupWindowHostView()
        view.setupShowing = setupShowing
        return view
    }

    func updateNSView(_ nsView: JournalSetupWindowHostView, context: Context) {
        nsView.setupShowing = setupShowing
    }
}

final class JournalSetupWindowHostView: NSView {
    var setupShowing: Bool = false {
        didSet {
            applyConfigurator()
        }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        applyConfigurator()
    }

    private func applyConfigurator() {
        guard let window else { return }
        JournalSetupWindowConfigurator.apply(setupShowing: setupShowing, to: window)
    }
}
