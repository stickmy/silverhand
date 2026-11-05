import XCTest

@testable import SilverhandNative

#if os(iOS)
  import UIKit
  import WebKit
  #if canImport(App)
    @testable import App
  #endif
#endif

/// Requires the isolated simulator CA and scripts/ios-acceptance-server.mjs.
@MainActor
final class NetworkAcceptanceTests: XCTestCase {
  #if canImport(App)
    func testAttachmentExportPreservesBytesAndCleansUpOnce() throws {
      let bytes = Data([0, 255, 128, 1])
      var results: [Bool] = []
      let picker = try AttachmentExportController(bytes: bytes, filename: "报告.zip") { results.append($0) }
      XCTAssertEqual(picker.sourceURL.lastPathComponent, "报告.zip")
      XCTAssertEqual(try Data(contentsOf: picker.sourceURL), bytes)
      picker.documentPicker(picker, didPickDocumentsAt: [URL(fileURLWithPath: "/destination/报告.zip")])
      picker.documentPickerWasCancelled(picker)
      XCTAssertEqual(results, [true])
      XCTAssertFalse(FileManager.default.fileExists(atPath: picker.sourceURL.deletingLastPathComponent().path))
    }

    func testAttachmentExportCancelAndPathContainment() throws {
      var results: [Bool] = []
      let picker = try AttachmentExportController(bytes: Data(), filename: "../../outside.txt") { results.append($0) }
      XCTAssertEqual(picker.sourceURL.lastPathComponent, "outside.txt")
      XCTAssertTrue(picker.sourceURL.deletingLastPathComponent().lastPathComponent.hasPrefix("silverhand-export-"))
      picker.documentPickerWasCancelled(picker)
      picker.documentPickerWasCancelled(picker)
      XCTAssertEqual(results, [false])
      XCTAssertFalse(FileManager.default.fileExists(atPath: picker.sourceURL.path))
    }

    func testAttachmentExportUnpresentedDeallocationRemovesTemporaryFile() throws {
      var picker: AttachmentExportController? = try AttachmentExportController(bytes: Data([1]), filename: "file.bin") { _ in }
      let url = picker!.sourceURL
      picker = nil
      XCTAssertFalse(FileManager.default.fileExists(atPath: url.path))
    }

    func testAttachmentExportProgrammaticDismissalCompletesCancellation() async throws {
      let window = UIWindow(frame: UIScreen.main.bounds)
      let host = UIViewController()
      window.rootViewController = host
      window.makeKeyAndVisible()
      defer { window.isHidden = true }
      var results: [Bool] = []
      let picker = try AttachmentExportController(bytes: Data([1]), filename: "file.bin") { results.append($0) }
      await withCheckedContinuation { continuation in
        host.present(picker, animated: false) { continuation.resume() }
      }
      await withCheckedContinuation { continuation in
        host.dismiss(animated: false) { continuation.resume() }
      }
      XCTAssertEqual(results, [false])
      XCTAssertFalse(FileManager.default.fileExists(atPath: picker.sourceURL.path))
    }
  #endif

  private func json(_ value: [String: Any]) throws -> String {
    String(data: try JSONSerialization.data(withJSONObject: value), encoding: .utf8)!
  }
  private func body(_ response: [String: Any]) throws -> [String: Any] {
    XCTAssertTrue(
      (200..<300).contains(response["status"] as? Int ?? 0),
      "HTTP status \(response["status"] ?? 0): \(response["bodyText"] ?? "")")
    return try JSONSerialization.jsonObject(
      with: Data((response["bodyText"] as? String ?? "{}").utf8)) as? [String: Any] ?? [:]
  }
  private func waitFor(_ predicate: () -> Bool) async throws {
    for _ in 0..<100 {
      if predicate() { return }
      try await Task.sleep(nanoseconds: 100_000_000)
    }
    XCTFail("Timed out waiting for authenticated WS frame")
  }

  func testNativeHTTPSAndAuthenticatedWSMessageLifecycle() async throws {
    #if !os(iOS)
      throw XCTSkip("Requires isolated iOS simulator with test CA and acceptance server")
    #else
      let store = KeychainSessionStore(service: "com.silverhand.acceptance.\(UUID().uuidString)")
      defer { try? store.write(nil) }
      let client = try ClientSession(store: store)
      _ = try await client.saveProfile("https://localhost:19443")
      let name =
        "ios" + UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(12).lowercased()
      let password = "Acceptance-2663-only!"
      let signup = try await client.authenticate(
        action: "signup",
        body: json(["email": "\(name)@example.test", "name": name, "password": password]))
      XCTAssertEqual(signup["ok"] as? Bool, true)
      XCTAssertNil(signup["user_token"])
      XCTAssertNotNil(try store.read()?.token)
      let hosts = try body(await client.proxy(path: "/api/hosts"))
      XCTAssertEqual(
        (hosts["hosts"] as? [Any])?.count, 0, "An iOS signup must not create a computer")
      let restored = try ClientSession(store: store)
      let restoredState = try await restored.authState()
      XCTAssertEqual((restoredState["me"] as? [String: Any])?["status"] as? Int, 200)
      let workspaceBody = try body(
        await client.proxy(path: "/api/workspaces", method: "POST", body: json(["name": name])))
      let workspace = workspaceBody["workspace"] as? [String: Any] ?? workspaceBody
      let workspaceID = try XCTUnwrap(workspace["id"] as? String)
      let channelBody = try body(
        await client.proxy(
          path: "/api/workspaces/\(workspaceID)/channels", method: "POST",
          body: json(["title": "iOS acceptance"])))
      let channel = channelBody["channel"] as? [String: Any] ?? channelBody
      let channelID = try XCTUnwrap(channel["id"] as? String)
      let messagesPath = "/api/channels/\(channelID)/messages"
      var frames: [[String: Any]] = []
      client.onSocket = { event in
        if let data = event["data"] as? String,
          let object = try? JSONSerialization.jsonObject(with: Data(data.utf8)) as? [String: Any]
        {
          frames.append(object)
        }
      }
      try client.openSocket(id: "first", url: "wss://localhost:19443/api/ws/events")
      try await waitFor { frames.contains { $0["type"] as? String == "authenticated" } }
      try await client.sendSocket(
        id: "first", data: json(["type": "subscribe", "scope": "conversation:\(channelID)"]))
      try await waitFor { frames.contains { $0["type"] as? String == "subscribed" } }
      _ = try body(
        await client.proxy(
          path: messagesPath, method: "POST", body: json(["content": "native live message"])))
      try await waitFor { frames.contains { ($0["type"] as? String) == "event" } }
      let history = try await client.proxy(path: messagesPath)
      XCTAssertTrue((history["bodyText"] as? String ?? "").contains("native live message"))
      client.setActive(false)
      do {
        try client.openSocket(id: "background", url: "wss://localhost:19443/api/ws/events")
        XCTFail("must not connect in background")
      } catch {}
      _ = try body(
        await restored.proxy(
          path: messagesPath, method: "POST", body: json(["content": "message while suspended"])))
      client.setActive(true)
      frames.removeAll()
      try client.openSocket(id: "resumed", url: "wss://localhost:19443/api/ws/events")
      try await waitFor { frames.contains { $0["type"] as? String == "authenticated" } }
      let resumedHistory = try await client.proxy(path: messagesPath)
      XCTAssertTrue(
        (resumedHistory["bodyText"] as? String ?? "").contains("message while suspended"))
      _ = try body(
        await client.proxy(
          path: "/api/me/password", method: "POST",
          body: json(["current_password": password, "new_password": password + "changed"])))
      XCTAssertNil(try store.read()?.token)
      let login = try await client.authenticate(
        action: "login",
        body: json(["email": "\(name)@example.test", "password": password + "changed"]))
      XCTAssertEqual(login["ok"] as? Bool, true)
      try await client.logout()
      XCTAssertNil(try store.read()?.token)
      _ = try client.clearProfile()
      XCTAssertNil(try store.read())
    #endif
  }
  #if os(iOS)
    private func findWebView(_ view: UIView) -> WKWebView? {
      if let web = view as? WKWebView { return web }
      for child in view.subviews { if let web = findWebView(child) { return web } }
      return nil
    }

    func testWebViewBridgeAndSharedUIMessageRoundTrip() async throws {
      var web: WKWebView?
      for _ in 0..<100 {
        for scene in UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }) {
          for window in scene.windows { if let found = findWebView(window) { web = found } }
        }
        if web != nil { break }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      let view = try XCTUnwrap(web)
      let tabs = try XCTUnwrap(
        UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.flatMap {
          $0.windows
        }.compactMap { $0.rootViewController as? UITabBarController }.first)
      func selectTab(_ index: Int) {
        _ = tabs.delegate?.tabBarController?(tabs, shouldSelect: tabs.viewControllers![index])
      }

      for _ in 0..<100 {
        if (try? await view.evaluateJavaScript(
          "typeof window.Capacitor?.nativePromise === 'function'")) as? Bool == true
        {
          break
        }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      func call(_ method: String, _ options: [String: Any] = [:]) async throws -> [String: Any] {
        try await view.callAsyncJavaScript(
          "return await window.Capacitor.nativePromise('SilverhandClient', method, options)",
          arguments: ["method": method, "options": options], in: nil, contentWorld: .page)
          as? [String: Any] ?? [:]
      }
      _ = try await call("clearProfile")
      _ = try await call("saveProfile", ["serverUrl": "https://localhost:19443"])
      let name =
        "bridge" + UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(10).lowercased()
      let auth = try await call(
        "authenticate",
        [
          "action": "signup",
          "body": json([
            "email": "\(name)@example.test", "name": name, "password": "Isolated-bridge-2663!",
          ]),
        ])
      XCTAssertEqual(auth["ok"] as? Bool, true)
      XCTAssertNil(auth["user_token"])
      let workspace = try body(
        await call(
          "request",
          ["path": "/api/workspaces", "method": "POST", "bodyText": json(["name": name])]))
      let workspaceID = try XCTUnwrap(
        (workspace["workspace"] as? [String: Any] ?? workspace)["id"] as? String)
      let channel = try body(
        await call(
          "request",
          [
            "path": "/api/workspaces/\(workspaceID)/channels", "method": "POST",
            "bodyText": json(["title": "Bridge acceptance"]),
          ]))
      let channelID = try XCTUnwrap(
        (channel["channel"] as? [String: Any] ?? channel)["id"] as? String)
      for index in 0..<12 {
        _ = try await call(
          "request",
          [
            "path": "/api/channels/\(channelID)/messages", "method": "POST",
            "bodyText": json([
              "content": "Reading position \(index)\n"
                + String(repeating: "A line of history.\n", count: 8)
            ]),
          ])
      }
      _ = try await view.evaluateJavaScript(
        "window.location.hash = '/chat/w/\(workspaceID)/c/\(channelID)'; window.location.reload();")
      var composerReady = false
      for _ in 0..<150 {
        composerReady =
          ((try? await view.evaluateJavaScript("!!document.querySelector('textarea')")) as? Bool)
          == true
        if composerReady { break }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      XCTAssertTrue(composerReady, "Shared React channel composer must mount after native login")
      guard composerReady else { return }
      _ = try await view.evaluateJavaScript(
        "document.querySelector('button[aria-label=\"Hide sidebar\"]')?.click();")
      try await Task.sleep(nanoseconds: 300_000_000)
      let composerWidth =
        try await view.evaluateJavaScript(
          "document.querySelector('textarea').getBoundingClientRect().width") as? Double ?? 0
      XCTAssertGreaterThan(
        composerWidth, 180, "The iPhone composer must remain usable when the sidebar is dismissed")
      _ = try await view.evaluateJavaScript(
        "const field = document.querySelector('textarea'); Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(field, 'shared UI sent message'); field.dispatchEvent(new Event('input', {bubbles:true}));"
      )
      try await Task.sleep(nanoseconds: 100_000_000)
      _ = try await view.evaluateJavaScript(
        "document.querySelector('textarea').dispatchEvent(new KeyboardEvent('keydown', {key:'Enter',code:'Enter',bubbles:true}));"
      )
      var sent = false
      for _ in 0..<100 {
        let history = try await call("request", ["path": "/api/channels/\(channelID)/messages"])
        if (history["bodyText"] as? String ?? "").contains("shared UI sent message") {
          sent = true
          break
        }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      XCTAssertTrue(sent, "The shared composer must send through the native HTTP bridge")
      _ = try body(
        await call(
          "request",
          [
            "path": "/api/channels/\(channelID)/messages", "method": "POST",
            "bodyText": json(["content": "native WS delivered message"]),
          ]))
      var received = false
      for _ in 0..<100 {
        received =
          ((try? await view.evaluateJavaScript(
            "document.body.innerText.includes('native WS delivered message')")) as? Bool) == true
        if received { break }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      XCTAssertTrue(received, "Native WS events must reach the shared React message list")
      // Drive the notifications observed by the actual Capacitor plugin, not
      // ClientSession.setActive directly. The same DOM node and draft must live
      // through reconnect and a missed-message snapshot refresh.
      _ = try await view.evaluateJavaScript(
        "window.acceptanceComposer = document.querySelector('textarea'); Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(window.acceptanceComposer, 'unsent foreground draft'); window.acceptanceComposer.dispatchEvent(new Event('input', {bubbles:true}));"
      )
      try await Task.sleep(nanoseconds: 100_000_000)
      NotificationCenter.default.post(
        name: UIApplication.didEnterBackgroundNotification, object: nil)
      try await Task.sleep(nanoseconds: 200_000_000)
      _ = try body(
        await call(
          "request",
          [
            "path": "/api/channels/\(channelID)/messages", "method": "POST",
            "bodyText": json(["content": "message during suspension"]),
          ]))
      NotificationCenter.default.post(
        name: UIApplication.willEnterForegroundNotification, object: nil)
      var resumed = false
      for _ in 0..<100 {
        resumed =
          (try? await view.evaluateJavaScript(
            "document.body.innerText.includes('message during suspension')")) as? Bool == true
        if resumed { break }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      XCTAssertTrue(resumed, "Foreground notification must refresh the mounted message list")
      // After catch-up, another event must arrive through the resumed socket.
      try await Task.sleep(nanoseconds: 500_000_000)
      _ = try body(
        await call(
          "request",
          [
            "path": "/api/channels/\(channelID)/messages", "method": "POST",
            "bodyText": json(["content": "WS after foreground"]),
          ]))
      var liveAfterResume = false
      for _ in 0..<100 {
        liveAfterResume =
          (try? await view.evaluateJavaScript(
            "document.body.innerText.includes('WS after foreground')")) as? Bool == true
        if liveAfterResume { break }
        try await Task.sleep(nanoseconds: 100_000_000)
      }
      XCTAssertTrue(liveAfterResume, "Foreground must also restore live WS delivery")
      let resumedDraft =
        try await view.evaluateJavaScript(
          "document.querySelector('textarea').value") as? String
      let sameComposer =
        try await view.evaluateJavaScript(
          "window.acceptanceComposer === document.querySelector('textarea')") as? Bool
      XCTAssertEqual(resumedDraft, "unsent foreground draft")
      XCTAssertEqual(
        sameComposer, true,
        "Resuming must retain the composer instance and its in-flight send identity")
      // Exercise the production React navigation through its visible controls.
      func waitFor(_ expression: String) async throws {
        for _ in 0..<100 {
          if (try? await view.evaluateJavaScript(expression)) as? Bool == true { return }
          try await Task.sleep(nanoseconds: 100_000_000)
        }
        XCTFail("Timed out waiting for mobile UI: \(expression)")
      }
      try await waitFor("document.querySelectorAll('.mobile-tabbar button').length === 0")
      if #available(iOS 18.0, *) { XCTAssertTrue(tabs.isTabBarHidden) }
      selectTab(1)
      try await waitFor("location.hash === '#/activity'")

      selectTab(0)
      try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
      _ = try await view.evaluateJavaScript(
        "document.querySelector('button[aria-label=\"Channel settings\"]').click()")
      try await waitFor(
        "location.hash.includes('settings=') && !document.querySelector('textarea')")
      _ = try await view.evaluateJavaScript(
        "Array.from(document.querySelectorAll('section button')).find(b => /^Members \\d+$/.test(b.textContent.trim())).click()"
      )
      try await waitFor("location.hash.includes('settingsPage=members')")
      #if canImport(App)
        let navigationHost = try XCTUnwrap(tabs as? SilverhandNavigationController)
        _ = try await view.evaluateJavaScript(
          "window.memberField = document.querySelector('input[placeholder=\"Search members or add someone\"]'); window.memberField.focus(); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(window.memberField, 'unfinished member search'); window.memberField.dispatchEvent(new Event('input', {bubbles:true}))"
        )
        // Back from the native bar runs the page's own Back; the stack stays
        // one page deep.
        func tapBack() throws {
          let stack = try XCTUnwrap(navigationHost.selectedViewController as? UINavigationController)
          XCTAssertEqual(stack.viewControllers.count, 1)
          navigationHost.pageBar.onEvent?(["kind": "back"])
        }
        try await Task.sleep(nanoseconds: 350_000_000)
        try tapBack()
        try await waitFor(
          "location.hash.includes('settings=') && !location.hash.includes('settingsPage=')")
        try await Task.sleep(nanoseconds: 350_000_000)
        try tapBack()
        try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
        // Re-open to retain the button-return regression below.
        _ = try await view.evaluateJavaScript(
          "document.querySelector('button[aria-label=\"Channel settings\"]').click()")
        try await waitFor("location.hash.includes('settings=')")
        _ = try await view.evaluateJavaScript(
          "Array.from(document.querySelectorAll('section button')).find(b => /^Members \\d+$/.test(b.textContent.trim())).click()"
        )
        try await waitFor("location.hash.includes('settingsPage=members')")
      #else
        XCTFail("Hosted gesture acceptance requires the App module")
      #endif

      _ = try await view.evaluateJavaScript("document.querySelector('.mobile-back').click()")
      try await waitFor(
        "location.hash.includes('settings=') && !location.hash.includes('settingsPage=') && !document.querySelector('textarea')"
      )
      _ = try await view.evaluateJavaScript("document.querySelector('.mobile-back').click()")
      try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
      selectTab(3)
      try await waitFor("location.hash === '#/me'")
      XCTAssertEqual(tabs.selectedIndex, 3)
      _ = try await view.evaluateJavaScript(
        "Array.from(document.querySelectorAll('[data-sidebar=me] button')).find(b => b.textContent.startsWith('Members')).click()"
      )
      try await waitFor("location.hash === '#/members'")
      XCTAssertEqual(tabs.selectedIndex, 3)
      _ = try await view.evaluateJavaScript("document.querySelector('.mobile-back').click()")
      try await waitFor("location.hash === '#/me'")
      selectTab(0)
      try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
      try await waitFor("!!document.querySelector('button[aria-label=\"Thread\"]')")
      _ = try await view.evaluateJavaScript(
        "window.threadRowId = Array.from(document.querySelectorAll('button[aria-label=\"Thread\"]')).find(b => { const row = b.closest('[data-message-row]').getBoundingClientRect(); const viewport = document.querySelector('[data-message-scroller]').getBoundingClientRect(); return row.top >= viewport.top && row.bottom <= viewport.bottom; }).closest('[data-message-row]').id; document.getElementById(window.threadRowId).querySelector('button[aria-label=\"Thread\"]').click()"
      )
      try await waitFor(
        "location.hash.includes('thread=') && document.querySelectorAll('textarea').length === 1")
      let threadDraft =
        try await view.evaluateJavaScript("document.querySelector('textarea').value") as? String
      XCTAssertEqual(threadDraft, "", "A thread must not inherit its parent's unsent draft")
      _ = try await view.evaluateJavaScript(
        "const threadField = document.querySelector('textarea'); Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(threadField, 'thread-only draft'); threadField.dispatchEvent(new Event('input', {bubbles:true}))"
      )
      try await Task.sleep(nanoseconds: 100_000_000)
      _ = try await view.evaluateJavaScript("document.querySelector('.mobile-back').click()")
      try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
      try await waitFor("!!document.querySelector('button[aria-label=\"Thread\"]')")
      try await waitFor("!!document.getElementById(window.threadRowId)")
      _ = try await view.evaluateJavaScript(
        "document.getElementById(window.threadRowId).querySelector('button[aria-label=\"Thread\"]').click()"
      )
      try await waitFor("document.querySelector('textarea')?.value === 'thread-only draft'")
      _ = try await view.evaluateJavaScript("document.querySelector('.mobile-back').click()")
      try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
      // Main-list pages use the full viewport.
      _ = try await view.evaluateJavaScript("document.querySelector('.mobile-back').click()")
      try await waitFor(
        "location.hash === '#/chat' && !!document.querySelector('[data-sidebar=workspaces]')")
      let listWidth =
        try await view.evaluateJavaScript(
          "document.querySelector('[data-sidebar=workspaces]').getBoundingClientRect().width / innerWidth"
        ) as? Double ?? 0
      XCTAssertGreaterThan(
        listWidth, 0.9, "The phone chat list must not retain the desktop sidebar width")
      _ = try await view.evaluateJavaScript(
        "location.hash = '/chat/w/\(workspaceID)/c/\(channelID)'")
      try await waitFor("document.querySelector('textarea')?.value === 'unsent foreground draft'")
      let horizontalOverflow =
        try await view.evaluateJavaScript("document.documentElement.scrollWidth > innerWidth")
        as? Bool
      XCTAssertEqual(horizontalOverflow, false, "Compact navigation must fit the phone viewport")
      try await waitFor("!!document.querySelector('[data-message-scroller]')")
      _ = try await view.evaluateJavaScript(
        "const scroller = document.querySelector('[data-message-scroller]'); scroller.scrollTop = scroller.scrollHeight / 2"
      )
      try await Task.sleep(nanoseconds: 250_000_000)
      _ = try await view.evaluateJavaScript(
        "const viewport = document.querySelector('[data-message-scroller]').getBoundingClientRect(); window.readingAnchor = Array.from(document.querySelectorAll('[data-message-row]')).find(e => e.getBoundingClientRect().top >= viewport.top && e.getBoundingClientRect().top < viewport.bottom)?.id"
      )
      try await waitFor("!!window.readingAnchor")
      selectTab(1)
      try await waitFor("location.hash === '#/activity'")
      selectTab(0)
      try await waitFor(
        "(() => { const item = document.getElementById(window.readingAnchor); const list = document.querySelector('[data-message-scroller]'); return !!item && !!list && item.getBoundingClientRect().bottom > list.getBoundingClientRect().top && item.getBoundingClientRect().top < list.getBoundingClientRect().bottom; })()"
      )
      _ = try await view.evaluateJavaScript(
        "document.querySelector('[data-scroll-bottom]')?.click()")
      try await waitFor("document.body.innerText.includes('WS after foreground')")
      // Each shipped theme must resolve the same navigation surface tokens.
      for theme in ["studio-light", "poster", "slack-dark", "iris", "glass"] {
        _ = try await view.evaluateJavaScript("document.documentElement.dataset.theme = '\(theme)'")
        try await Task.sleep(nanoseconds: 150_000_000)
        let themeShot = UIGraphicsImageRenderer(bounds: view.bounds).image { _ in
          view.drawHierarchy(in: view.bounds, afterScreenUpdates: true)
        }
        let attachment = XCTAttachment(image: themeShot)
        attachment.name = "mobile-chat-\(theme)"
        attachment.lifetime = .keepAlways
        add(attachment)
      }
      let snapshot = UIGraphicsImageRenderer(bounds: view.bounds).image { _ in
        view.drawHierarchy(in: view.bounds, afterScreenUpdates: true)
      }
      let screenshot = XCTAttachment(image: snapshot)
      screenshot.lifetime = .keepAlways
      add(screenshot)
      selectTab(2)
      try await waitFor(
        "location.hash === '#/tasks' && !!document.querySelector('[data-task-board-toolbar]')")
      if #available(iOS 18.0, *) { XCTAssertFalse(tabs.isTabBarHidden) }
      let nativeRootShot = UIGraphicsImageRenderer(bounds: tabs.view.bounds).image { _ in
        tabs.view.drawHierarchy(in: tabs.view.bounds, afterScreenUpdates: true)
      }
      let nativeRootAttachment = XCTAttachment(image: nativeRootShot)
      nativeRootAttachment.name = "native-liquid-glass-tasks"
      nativeRootAttachment.lifetime = .keepAlways
      add(nativeRootAttachment)
      let filtersUseTwoColumns =
        try await view.evaluateJavaScript(
          "getComputedStyle(document.querySelector('[data-task-board-toolbar] [data-task-filter-bar]')).gridTemplateColumns.split(' ').length === 2"
        ) as? Bool
      XCTAssertEqual(filtersUseTwoColumns, true)
      let createHasOwnRow =
        try await view.evaluateJavaScript(
          "(() => { const toolbar = document.querySelector('[data-task-board-toolbar]'); return toolbar.querySelector(':scope > button').getBoundingClientRect().top >= toolbar.querySelector('[data-task-filter-bar]').getBoundingClientRect().bottom; })()"
        ) as? Bool
      XCTAssertEqual(createHasOwnRow, true)
      // Native controls must respect the same nested Web modal boundary.
      _ = try await view.evaluateJavaScript(
        "document.querySelector('[data-task-board-toolbar] > button').click()")
      try await waitFor("document.querySelectorAll('[role=dialog]').length === 1")
      _ = try await view.evaluateJavaScript(
        "const title = document.querySelector('[role=dialog] input'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(title, 'modal draft'); title.dispatchEvent(new Event('input', {bubbles:true})); Array.from(document.querySelectorAll('[role=dialog] button')).find(b => b.textContent.includes('Select a channel')).click()"
      )
      try await waitFor("document.querySelectorAll('[role=dialog]').length === 2")
      try await Task.sleep(nanoseconds: 100_000_000)
      selectTab(0)
      #if canImport(App)
        // A modal leaves nothing beneath the page to swipe back to.
        let modalStack = try XCTUnwrap(
          (tabs as? SilverhandNavigationController)?.selectedViewController
            as? UINavigationController)
        XCTAssertEqual(modalStack.viewControllers.count, 1)
      #endif
      _ = try await view.evaluateJavaScript(
        "Array.from(document.querySelectorAll('[role=dialog]')).at(-1).querySelector('[data-sticker-close]').click()"
      )
      try await waitFor("document.querySelectorAll('[role=dialog]').length === 1")
      selectTab(0)
      try await Task.sleep(nanoseconds: 100_000_000)
      let modalRoute = try await view.evaluateJavaScript("location.hash") as? String
      XCTAssertEqual(modalRoute, "#/tasks")
      let modalDraft =
        try await view.evaluateJavaScript("document.querySelector('[role=dialog] input').value")
        as? String
      XCTAssertEqual(modalDraft, "modal draft")
      _ = try await view.evaluateJavaScript(
        "document.querySelector('[role=dialog] [data-sticker-close]').click()")
      try await waitFor("!document.querySelector('[role=dialog]')")
      try await Task.sleep(nanoseconds: 100_000_000)
      selectTab(0)
      try await waitFor("location.hash !== '#/tasks'")
      selectTab(2)
      try await waitFor("!!document.querySelector('[data-task-board-toolbar] > button')")
      _ = try await view.evaluateJavaScript(
        "document.querySelector('[data-task-board-toolbar] > button').click()")
      try await waitFor("!!document.querySelector('[role=dialog]')")
      // An explicit page replacement unmounts the form; ownership must release.
      _ = try await view.evaluateJavaScript("location.hash = '/activity'")
      try await waitFor(
        "location.hash === '#/activity' && !document.querySelector('[role=dialog]')")
      try await Task.sleep(nanoseconds: 100_000_000)
      selectTab(2)
      try await waitFor("location.hash === '#/tasks'")

      _ = try await call("logout")
      _ = try await view.evaluateJavaScript("location.reload()")
      try await waitFor(
        "!!document.querySelector('input[autocomplete=\"current-password\"]') && !!document.querySelector('button[aria-label=\"Change server\"]')"
      )
      _ = try await view.evaluateJavaScript(
        "for (const [selector, value] of [['#auth-email', 'isolated@example.test'], ['input[autocomplete=\"current-password\"]', 'test-password']]) { const field = document.querySelector(selector); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(field, value); field.dispatchEvent(new Event('input', {bubbles:true})); }"
      )
      try await waitFor("!document.querySelector('button[type=submit]').disabled")
      _ = try await view.evaluateJavaScript(
        "document.querySelector('button[aria-label=\"Change server\"]').click()")
      try await waitFor(
        "!!document.querySelector('#login-server') && document.querySelector('button[type=submit]').disabled"
      )
      _ = try await view.evaluateJavaScript(
        "const address = document.querySelector('#login-server'); Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(address, 'invalid address'); address.dispatchEvent(new Event('input', {bubbles:true}))"
      )
      let enterPrevented =
        try await view.evaluateJavaScript(
          "!document.querySelector('#login-server').dispatchEvent(new KeyboardEvent('keydown', {key:'Enter', bubbles:true, cancelable:true}))"
        ) as? Bool
      XCTAssertEqual(
        enterPrevented, true, "Address Enter must not submit the outer credential form")
      try await waitFor("document.body.innerText.includes('Enter a valid HTTPS server address.')")
      try await waitFor("document.querySelector('button[type=submit]').disabled")
      _ = try await view.evaluateJavaScript(
        "Array.from(document.querySelectorAll('button')).find(b => b.textContent === 'Cancel').click()"
      )
      try await waitFor(
        "!document.querySelector('#login-server') && !document.querySelector('button[type=submit]').disabled && !document.body.innerText.includes('Enter a valid HTTPS server address.')"
      )
      _ = try await call("clearProfile")
      _ = try await view.evaluateJavaScript("location.hash = '/chat'; location.reload()")
      try await waitFor("!!document.querySelector('input[autocomplete=\"current-password\"]')")
      let legacyConnect =
        try await view.evaluateJavaScript("!!document.querySelector('.ios-server-setup')") as? Bool
      XCTAssertEqual(legacyConnect, false, "A fresh install must open login directly")
      try await waitFor("document.body.innerText.includes('https://silverhand.ai')")
    }
  #endif

}
