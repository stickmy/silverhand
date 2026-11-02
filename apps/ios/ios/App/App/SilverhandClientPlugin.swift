import Capacitor
import SilverhandNative
import UIKit
import WebKit

@objc(SilverhandClientPlugin)
public final class SilverhandClientPlugin: CAPPlugin, CAPBridgedPlugin {
  public let identifier = "SilverhandClientPlugin"
  public let jsName = "SilverhandClient"
  public let pluginMethods: [CAPPluginMethod] = [
    "getProfile", "saveProfile", "clearProfile", "authState", "serverInfo", "authenticate",
    "passwordRecovery", "logout", "request",
    "upload",
    "openSocket", "sendSocket", "closeSocket", "openExternal", "appVersion",
    "navigationSync", "navigationReset",
    "navigationBadges", "navigationAvatar", "setLocale", "titlebar", "titlebarEndSearch", "pageBar", "pageBarBackProgress", "composerSync", "composerRemove", "composerSetText", "selectionBarSync", "selectionBarRemove", "bannerShow", "sharePng", "previewPng", "saveAttachment", "avatarMakerEdit", "contextMenuAvailable", "contextMenuShow",
  ].map { CAPPluginMethod(name: $0, returnType: CAPPluginReturnPromise) }

  @MainActor private var session: ClientSession?
  /// The current value of each state event (bar metrics, the fold). A page
  /// loaded again (logout, a new server) listens anew while native still
  /// holds the same value, so every new listener gets it first.
  @MainActor private var states: [String: [String: Any]] = [:]
  private var observers: [NSObjectProtocol] = []

  @MainActor func publishState(_ event: String, _ data: [String: Any]) {
    states[event] = data
    notifyListeners(event, data: data)
  }

  public override func addEventListener(_ eventName: String, listener: CAPPluginCall) {
    super.addEventListener(eventName, listener: listener)
    Task { @MainActor in
      if let data = states[eventName] { notifyListeners(eventName, data: data) }
    }
  }

  public override func load() {
    observers.append(
      NotificationCenter.default.addObserver(
        forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: .main
      ) { [weak self] _ in
        Task { @MainActor in self?.session?.setActive(false) }
      })
    observers.append(
      NotificationCenter.default.addObserver(
        forName: UIApplication.willEnterForegroundNotification, object: nil, queue: .main
      ) { [weak self] _ in
        Task { @MainActor in
          self?.session?.setActive(true)
          self?.notifyListeners("foreground", data: [:])
        }
      })
    observers.append(
      NotificationCenter.default.addObserver(
        forName: UIResponder.keyboardWillChangeFrameNotification, object: nil, queue: .main
      ) { [weak self] notification in
        guard let frame = notification.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect
        else { return }
        Task { @MainActor in
          guard let self, let window = self.bridge?.viewController?.view.window else { return }
          let keyboard = window.convert(frame, from: window.screen.coordinateSpace)
          let overlap = window.bounds.intersection(keyboard)
          let docked = !overlap.isNull && overlap.height > 0
            && keyboard.maxY >= window.bounds.maxY
          self.notifyListeners("keyboardChanged", data: ["docked": docked])
        }
      })
  }

  deinit { for observer in observers { NotificationCenter.default.removeObserver(observer) } }

  @objc func saveAttachment(_ call: CAPPluginCall) {
    Task { @MainActor in
      guard let encoded = call.getString("base64"), let bytes = Data(base64Encoded: encoded),
        let filename = call.getString("filename"),
        let host = self.bridge?.viewController, host.view.window != nil,
        host.presentedViewController == nil
      else {
        call.reject("Unable to present file saving")
        return
      }
      do {
        let picker = try AttachmentExportController(bytes: bytes, filename: filename) { completed in
          call.resolve(["completed": completed])
        }
        host.present(picker, animated: true)
      } catch { call.reject(error.localizedDescription) }
    }
  }

  @objc func contextMenuAvailable(_ call: CAPPluginCall) {
    call.resolve(["available": SilverhandContextMenu.isAvailable])
  }

  @objc func contextMenuShow(_ call: CAPPluginCall) {
    let menu = call.getObject("menu") ?? [:]
    let point = menu["point"] as? JSObject ?? [:]
    let request = ContextMenuRequest(
      point: CGPoint(x: point["x"] as? Double ?? 0, y: point["y"] as? Double ?? 0),
      reactions: (menu["reactions"] as? JSArray ?? []).compactMap { value in
        guard let item = value as? JSObject, let emoji = item["emoji"] as? String else { return nil }
        return (emoji, item["label"] as? String ?? emoji)
      },
      sections: (menu["sections"] as? JSArray ?? []).map { section in
        (section as? JSArray ?? []).compactMap { value in
          guard let item = value as? JSObject, let id = item["id"] as? String else { return nil }
          return ContextMenuItem(
            id: id, label: item["label"] as? String ?? "", icon: item["icon"] as? String ?? "")
        }
      })
    Task { @MainActor in
      guard let web = self.bridge?.webView else {
        call.reject("Unable to present the menu")
        return
      }
      let shown = SilverhandContextMenu.present(request, over: web) { choice in
        switch choice {
        case .item(let id): call.resolve(["choice": ["kind": "item", "id": id]])
        case .react(let emoji): call.resolve(["choice": ["kind": "react", "emoji": emoji]])
        case nil: call.resolve([:])
        }
      }
      if !shown { call.reject("Unable to present the menu") }
    }
  }

  @objc func avatarMakerEdit(_ call: CAPPluginCall) {
    let config = call.getObject("config") ?? [:]
    let parts = (config["parts"] as? [String: Any] ?? [:]).compactMapValues { $0 as? Int }
    let bg = config["bg"] as? Int ?? 0
    Task { @MainActor in
      guard let host = self.bridge?.viewController, host.view.window != nil,
        host.presentedViewController == nil
      else {
        call.reject("Unable to present the avatar editor")
        return
      }
      do {
        try SilverhandAvatarMaker.present(from: host, parts: parts, bg: bg) { result in
          guard let result else {
            call.resolve([:])
            return
          }
          call.resolve([
            "config": ["parts": result.config.parts, "bg": result.config.bg],
            "base64": result.png.base64EncodedString(),
          ])
        }
      } catch { call.reject(error.localizedDescription) }
    }
  }

  /// The image comes inline (`base64`) or as an attachment native downloads
  /// itself (`attachmentId`), so the sheet never waits on the page for bytes.
  @objc func previewPng(_ call: CAPPluginCall) {
    Task { @MainActor in
      let inline = call.getString("base64").flatMap { Data(base64Encoded: $0) }
      let attachment = call.getString("attachmentId")
      guard inline != nil || attachment != nil,
        let host = self.bridge?.viewController,
        host.view.window != nil,
        host.presentedViewController == nil
      else {
        call.reject("Unable to present the image preview")
        return
      }
      ImagePreviewController.present(
        title: call.getString("title") ?? "", shareLabel: call.getString("shareLabel") ?? "",
        closeLabel: call.getString("closeLabel") ?? "", from: host,
        load: {
          if let inline { return inline }
          let id = attachment?.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? ""
          return try await self.client().download(path: "/api/attachments/\(id)")
        }
      ) { result in
        switch result {
        case .success(let shared): call.resolve(["shared": shared])
        case .failure(let error): call.reject(error.localizedDescription)
        }
      }
    }
  }

  @objc func sharePng(_ call: CAPPluginCall) {
    Task { @MainActor in
      guard let encoded = call.getString("base64"),
        let bytes = Data(base64Encoded: encoded),
        let image = UIImage(data: bytes),
        let host = self.bridge?.viewController,
        host.view.window != nil,
        host.presentedViewController == nil
      else {
        call.reject("Unable to present image sharing")
        return
      }

      // UIKit owns the image for the duration of sharing. No temporary file or
      // alternative file-picker lifecycle is introduced.
      let activity = UIActivityViewController(activityItems: [image], applicationActivities: nil)
      activity.completionWithItemsHandler = { _, completed, _, error in
        if let error { call.reject(error.localizedDescription) }
        else { call.resolve(["completed": completed]) }
      }
      if let popover = activity.popoverPresentationController {
        popover.sourceView = host.view
        popover.sourceRect = CGRect(x: host.view.bounds.midX, y: host.view.bounds.maxY - 1, width: 1, height: 1)
        popover.permittedArrowDirections = []
      }
      host.present(activity, animated: true)
    }
  }

  @MainActor private func client() throws -> ClientSession {
    if let session { return session }
    let client = try ClientSession(defaultServerURL: getConfig().getString("defaultServerURL"))
    client.onSocket = { [weak self] event in self?.notifyListeners("socket", data: event) }
    client.onInvalidated = { [weak self] in
      self?.navigationHost?.invalidate()
      self?.notifyListeners("sessionInvalidated", data: [:])
    }
    session = client
    return client
  }

  private func run(
    _ call: CAPPluginCall,
    operation: @escaping @MainActor (ClientSession) async throws -> [String: Any]
  ) {
    Task { @MainActor in
      do { call.resolve(try await operation(client())) } catch {
        call.reject(error.localizedDescription)
      }
    }
  }

  @MainActor private var navigationHost: SilverhandNavigationController? {
    var controller: UIViewController? = bridge?.viewController
    while let current = controller {
      if let host = current as? SilverhandNavigationController { return host }
      controller = current.parent
    }
    return nil
  }

  @MainActor private func bindNavigationEvents(to host: SilverhandNavigationController) {
    host.onIntent = { [weak self] intent in self?.notifyListeners("navigation", data: intent) }
    host.onComposerEvent = { [weak self] event in self?.notifyListeners("composer", data: event) }
  }

  @objc public func setLocale(_ call: CAPPluginCall) {
    Task { @MainActor in
      if NativeCopy.set(call.getString("locale")) {
        navigationHost?.applyLocale()
      }
      call.resolve()
    }
  }
  @objc public func navigationBadges(_ call: CAPPluginCall) {
    Task { @MainActor in
      navigationHost?.badges(call)
      call.resolve()
    }
  }
  @objc public func navigationSync(_ call: CAPPluginCall) {
    Task { @MainActor in
      guard let host = navigationHost else {
        call.reject("Native navigation host unavailable")
        return
      }
      bindNavigationEvents(to: host)
      call.resolve(["tabBarHeight": host.sync(call)])
    }
  }
  @objc public func navigationAvatar(_ call: CAPPluginCall) {
    let image = call.getString("base64").flatMap { Data(base64Encoded: $0) }.flatMap(UIImage.init)
    Task { @MainActor in
      navigationHost?.setAvatar(image)
      call.resolve()
    }
  }
  @objc public func titlebar(_ call: CAPPluginCall) {
    let state = call.getObject("state").map { state in
      TitlebarState(
        sidebar: (state["sidebar"] as? JSObject).map {
          .init(shown: $0["shown"] as? Bool ?? false, label: $0["label"] as? String ?? "")
        },
        status: state["status"] as? String,
        searchPlaceholder: state["searchPlaceholder"] as? String ?? "",
        searchShortcut: state["searchShortcut"] as? String ?? "",
        dark: state["dark"] as? Bool ?? false,
        actions: (state["actions"] as? JSArray ?? []).compactMap { value in
          guard let item = value as? JSObject, let id = item["id"] as? String else { return nil }
          return BarAction(
            id: id, label: item["label"] as? String ?? "",
            accessibilityLabel: item["accessibilityLabel"] as? String,
            icon: item["icon"] as? String ?? "",
            selected: item["selected"] as? Bool ?? false, dot: item["dot"] as? Bool ?? false)
        })
    }
    Task { @MainActor [self] in
      if let host = navigationHost {
        host.titlebar.state = state
        host.titlebar.onEvent = { [weak self] event in self?.notifyListeners("titlebar", data: event) }
        host.applyBars()
        host.observeContentScroll()
      }
      call.resolve()
    }
  }
  @objc public func pageBar(_ call: CAPPluginCall) {
    func action(_ value: Any) -> BarAction? {
      guard let item = value as? JSObject, let id = item["id"] as? String else { return nil }
      return BarAction(
        id: id, label: item["label"] as? String ?? "",
        accessibilityLabel: item["accessibilityLabel"] as? String,
        icon: item["icon"] as? String ?? "",
        selected: item["selected"] as? Bool ?? false, dot: item["dot"] as? Bool ?? false,
        destructive: item["destructive"] as? Bool ?? false,
        primary: item["primary"] as? Bool ?? false)
    }
    let state = call.getObject("state").map { state in
      PageBarState(
        title: state["title"] as? String ?? "",
        subtitle: state["subtitle"] as? String,
        titleAction: state["titleAction"] as? String,
        titleIcon: (state["titleIcon"] as? JSObject).flatMap { icon in
          (icon["glyph"] as? String).map(PageBarTitleIcon.glyph)
            ?? (icon["image"] as? String).map(PageBarTitleIcon.image)
        },
        backdrop: state["backdrop"] as? Bool ?? false,
        largeTitle: state["largeTitle"] as? Bool ?? false,
        back: state["back"] as? String,
        actions: (state["actions"] as? JSArray ?? []).map { ($0 as? JSArray ?? []).compactMap(action) }
          .filter { !$0.isEmpty },
        overflow: (state["overflow"] as? JSObject).map {
          BarAction(
            id: "more", label: $0["label"] as? String ?? "", icon: $0["icon"] as? String ?? "",
            selected: false, dot: false)
        },
        tabs: (state["tabs"] as? JSArray)?.compactMap { tab in
          action(tab).map {
            PageBarTab(item: $0, closeLabel: (tab as? JSObject)?["closeLabel"] as? String)
          }
        },
        addTabs: (state["addTabs"] as? JSObject).map {
          PageBarAddMenu(
            label: $0["label"] as? String ?? "",
            items: ($0["items"] as? JSArray ?? []).compactMap(action))
        })
    }
    Task { @MainActor [self] in
      if let host = navigationHost {
        host.pageBar.state = state
        host.pageBar.onEvent = { [weak self] event in self?.notifyListeners("pageBar", data: event) }
        host.applyBars()
        host.observeContentScroll()
      }
      call.resolve()
    }
  }
  @objc public func composerSync(_ call: CAPPluginCall) {
    guard let state = call.getObject("state"), let key = state["key"] as? String else {
      call.reject("Missing composer state")
      return
    }
    let next = ComposerState(
      key: key,
      placeholder: state["placeholder"] as? String ?? "",
      enabled: state["enabled"] as? Bool ?? false,
      canSend: state["canSend"] as? Bool ?? false,
      sendLabel: state["sendLabel"] as? String ?? "",
      addLabel: state["addLabel"] as? String ?? "",
      accessoryHeight: CGFloat(state["accessoryHeight"] as? Double ?? 0),
      add: (state["add"] as? JSArray ?? []).compactMap { value in
        guard let item = value as? JSObject, let id = item["id"] as? String else { return nil }
        return ComposerMenuItem(
          id: id, label: item["label"] as? String ?? "", icon: item["icon"] as? String ?? "",
          pick: item["pick"] as? String)
      },
      picker: (state["picker"] as? JSObject).map { picker in
        ComposerPicker(
          rows: (picker["rows"] as? JSArray ?? []).compactMap { value in
            guard let row = value as? JSObject, let id = row["id"] as? String,
              let icon = row["icon"] as? JSObject
            else { return nil }
            let face: ComposerPickerRow.Icon =
              (icon["image"] as? String).map { .image($0) }
              ?? .glyph(icon["glyph"] as? String ?? "", ground: icon["ground"] as? Bool ?? false)
            return ComposerPickerRow(
              id: id, title: row["title"] as? String ?? "", handle: row["handle"] as? String,
              detail: row["detail"] as? String, section: row["section"] as? String, icon: face,
              dimmed: row["dimmed"] as? Bool ?? false)
          },
          footer: picker["footer"] as? String)
      },
      activity: (state["activity"] as? JSObject).flatMap { activity in
        guard let id = activity["id"] as? String else { return nil }
        return ComposerActivity(
          id: id,
          faces: (activity["faces"] as? JSArray ?? []).compactMap { value in
            guard let icon = value as? JSObject else { return nil }
            return (icon["image"] as? String).map { .image($0) }
              ?? .glyph(icon["glyph"] as? String ?? "", ground: icon["ground"] as? Bool ?? false)
          },
          title: activity["title"] as? String ?? "", detail: activity["detail"] as? String ?? "",
          label: activity["label"] as? String ?? "", since: activity["since"] as? Double ?? 0)
      },
      frame: (state["frame"] as? JSObject).map {
        ComposerFrame(
          left: CGFloat($0["left"] as? Double ?? 0), width: CGFloat($0["width"] as? Double ?? 0),
          bottom: CGFloat($0["bottom"] as? Double ?? 0))
      })
    Task { @MainActor [self] in
      navigationHost?.showComposer(next)
      call.resolve()
    }
  }
  @objc public func selectionBarSync(_ call: CAPPluginCall) {
    guard let state = call.getObject("state"), let key = state["key"] as? String else {
      call.reject("Missing selection bar state")
      return
    }
    let next = SelectionBarState(
      key: key, count: state["count"] as? String ?? "",
      cancelLabel: state["cancelLabel"] as? String ?? "",
      copyLabel: state["copyLabel"] as? String ?? "",
      saveLabel: state["saveLabel"] as? String ?? "",
      enabled: state["enabled"] as? Bool ?? false, busy: state["busy"] as? Bool ?? false)
    Task { @MainActor [self] in
      if let host = navigationHost {
        host.selectionBar.onEvent = { [weak self] event in
          self?.notifyListeners("selectionBar", data: event)
        }
        host.showSelectionBar(next)
      }
      call.resolve()
    }
  }
  @objc public func selectionBarRemove(_ call: CAPPluginCall) {
    let key = call.getString("key")
    Task { @MainActor in
      navigationHost?.removeSelectionBar(key: key)
      call.resolve()
    }
  }
  @objc public func bannerShow(_ call: CAPPluginCall) {
    guard let id = call.getString("id"), let title = call.getString("title") else {
      call.reject("A banner needs an id and a title")
      return
    }
    let icon = call.getObject("icon").map { icon -> ComposerPickerRow.Icon in
      (icon["image"] as? String).map { .image($0) }
        ?? .glyph(icon["glyph"] as? String ?? "", ground: icon["ground"] as? Bool ?? false)
    }
    let notice = BannerNotice(
      id: id, title: title, subtitle: call.getString("subtitle"), body: call.getString("body"),
      icon: icon)
    Task { @MainActor [self] in
      if let host = navigationHost {
        host.banner.onEvent = { [weak self] event in self?.notifyListeners("banner", data: event) }
        host.showBanner(notice)
      }
      call.resolve()
    }
  }
  @objc public func composerRemove(_ call: CAPPluginCall) {
    let key = call.getString("key")
    Task { @MainActor in
      navigationHost?.removeComposer(key: key)
      call.resolve()
    }
  }
  @objc public func composerSetText(_ call: CAPPluginCall) {
    let key = call.getString("key") ?? ""
    let text = call.getString("text") ?? ""
    let caret = call.getInt("caret") ?? (text as NSString).length
    let focus = call.getBool("focus") ?? false
    Task { @MainActor in
      navigationHost?.setComposerText(key: key, text: text, caret: caret, focus: focus)
      call.resolve()
    }
  }
  @objc public func pageBarBackProgress(_ call: CAPPluginCall) {
    let progress = call.getDouble("progress").map { CGFloat($0) }
    Task { @MainActor in
      navigationHost?.setBackProgress(progress)
      call.resolve()
    }
  }
  @objc public func titlebarEndSearch(_ call: CAPPluginCall) {
    let clear = call.getBool("clear") ?? false
    Task { @MainActor in
      navigationHost?.titlebar.endSearch(clear: clear)
      call.resolve()
    }
  }
  @objc public func navigationReset(_ call: CAPPluginCall) {
    Task { @MainActor in
      navigationHost?.reset(session: call.getString("session") ?? "")
      call.resolve()
    }
  }

  @objc public func getProfile(_ call: CAPPluginCall) { run(call) { try $0.profile() } }
  @objc public func saveProfile(_ call: CAPPluginCall) {
    run(call) {
      let result = try await $0.saveProfile(call.getString("serverUrl") ?? "")
      self.navigationHost?.invalidate()
      return result
    }
  }
  @objc public func clearProfile(_ call: CAPPluginCall) {
    run(call) {
      let cleared = try $0.clearProfile()
      self.navigationHost?.invalidate()
      return ["cleared": cleared]
    }
  }
  @objc public func authState(_ call: CAPPluginCall) { run(call) { try await $0.authState() } }
  @objc public func serverInfo(_ call: CAPPluginCall) { run(call) { try await $0.serverInfo() } }
  @objc public func authenticate(_ call: CAPPluginCall) {
    run(call) {
      try await $0.authenticate(
        action: call.getString("action") ?? "", body: call.getString("body") ?? "{}")
    }
  }
  @objc public func passwordRecovery(_ call: CAPPluginCall) {
    run(call) {
      try await $0.passwordRecovery(
        action: call.getString("action") ?? "", body: call.getString("body") ?? "{}")
    }
  }
  @objc public func logout(_ call: CAPPluginCall) {
    run(call) {
      try await $0.logout()
      self.navigationHost?.invalidate()
      return [:]
    }
  }
  @objc public func request(_ call: CAPPluginCall) {
    run(call) {
      try await $0.proxy(
        path: call.getString("path") ?? "", method: call.getString("method") ?? "GET",
        body: call.getString("bodyText"),
        headers: call.getObject("headers") as? [String: String] ?? [:],
        binary: call.getString("responseType") == "arrayBuffer")
    }
  }
  @objc public func upload(_ call: CAPPluginCall) {
    let fields = (call.getArray("fields") as? [[String: Any]] ?? []).compactMap {
      field -> (name: String, value: String)? in
      guard let name = field["name"] as? String, let value = field["value"] as? String else {
        return nil
      }
      return (name, value)
    }
    let files = (call.getArray("files") as? [[String: Any]] ?? []).compactMap {
      file -> ClientSession.UploadFile? in
      guard let fieldName = file["fieldName"] as? String,
        let filename = file["filename"] as? String,
        let base64 = file["base64"] as? String, let data = Data(base64Encoded: base64)
      else { return nil }
      return ClientSession.UploadFile(
        fieldName: fieldName, filename: filename,
        contentType: file["contentType"] as? String ?? "application/octet-stream", data: data)
    }
    run(call) {
      try await $0.upload(path: call.getString("path") ?? "", fields: fields, files: files)
    }
  }
  @objc public func openSocket(_ call: CAPPluginCall) {
    run(call) {
      try $0.openSocket(id: call.getString("id") ?? "", url: call.getString("url") ?? "")
      return [:]
    }
  }
  @objc public func sendSocket(_ call: CAPPluginCall) {
    run(call) {
      try await $0.sendSocket(id: call.getString("id") ?? "", data: call.getString("data") ?? "")
      return [:]
    }
  }
  @objc public func closeSocket(_ call: CAPPluginCall) {
    run(call) {
      $0.closeSocket(id: call.getString("id") ?? "")
      return [:]
    }
  }
  @objc public func appVersion(_ call: CAPPluginCall) {
    let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
    call.resolve(["version": version ?? ""])
  }
  @objc public func openExternal(_ call: CAPPluginCall) {
    Task { @MainActor in
      guard let raw = call.getString("url"), let url = URL(string: raw),
        ["https", "http", "mailto"].contains(url.scheme?.lowercased() ?? "")
      else {
        call.reject("Unsupported external URL")
        return
      }
      let opened = await UIApplication.shared.open(url, options: [:])
      if opened { call.resolve() } else { call.reject("Unable to open this URL") }
    }
  }
}

/// The page lays out against the window's top and bottom insets alone. The
/// tab's bars draw over it and the page clears them itself, so showing or
/// hiding a bar never reflows the pages kept beneath the one on screen.
final class PageWebView: WKWebView {
  override var safeAreaInsets: UIEdgeInsets {
    var insets = super.safeAreaInsets
    if let window {
      insets.top = window.safeAreaInsets.top
      insets.bottom = window.safeAreaInsets.bottom
    }
    return insets
  }
}

class SilverhandViewController: CAPBridgeViewController {
  private struct Fold: Equatable {
    let axis: String
    let start, end: CGFloat
  }

  private struct BarMetrics: Equatable {
    let bottom, tabs: CGFloat
    let vertical: Bool
  }

  private let plugin = SilverhandClientPlugin()
  private var barMetrics: BarMetrics?
  // Last published value; the event fires only on change.
  private var fold: Fold?

  override func webView(with frame: CGRect, configuration: WKWebViewConfiguration) -> WKWebView {
    PageWebView(frame: frame, configuration: configuration)
  }

  override func capacitorDidLoad() {
    bridge?.registerPluginInstance(plugin)
    // Folding moves the division region without a layout pass, so a dialog
    // already open would keep its unfolded frame. Re-read on hinge changes.
    if #available(iOS 27.1, *) {
      view.addInteraction(
        UIHingeInteraction { [weak self] _, _ in
          DispatchQueue.main.async { self?.publishFold() }
        })
    }
  }

  override func viewDidLayoutSubviews() {
    super.viewDidLayoutSubviews()
    publishFold()
  }

  /// The active fold (iPhone Duo half open) as a band across the view,
  /// including the system margins around the crease. The band runs top to
  /// bottom when it is taller than wide.
  private func publishFold() {
    var next: Fold?
    if #available(iOS 27.1, *),
      let region = view.reservedRegions(kind: .division).first(where: { $0.isActive })
    {
      let band = region.frame
      let vertical = band.height > band.width
      next = Fold(
        axis: vertical ? "vertical" : "horizontal",
        start: vertical ? band.minX : band.minY,
        end: vertical ? band.maxX : band.maxY)
    }
    guard next != fold else { return }
    fold = next
    let data: [String: Any] = next.map { ["axis": $0.axis, "start": $0.start, "end": $0.end] } ?? [:]
    plugin.publishState("fold", data)
  }

  /// Where the page's bars end, for the page to clear them: `bottom` of the
  /// navigation bar (on iPhone Duo, of the title under it), the height of the
  /// view tabs under the bar, and whether the bars stand on a side edge
  /// (iPhone Duo).
  func publishBarMetrics(bottom: CGFloat, tabs: CGFloat, vertical: Bool) {
    let next = BarMetrics(bottom: bottom, tabs: tabs, vertical: vertical)
    guard next != barMetrics else { return }
    barMetrics = next
    plugin.publishState(
      "barMetrics", ["bottom": bottom, "tabs": tabs, "vertical": vertical])
  }
}

extension UITraitCollection {
  /// The side edge where iOS stands bars (iPhone Duo), or nil when they are
  /// horizontal.
  var tabBarEdge: NSDirectionalRectEdge? {
    guard #available(iOS 27.1, *) else { return nil }
    switch verticalBarEdge {
    case .leading: return .leading
    case .trailing: return .trailing
    default: return nil
    }
  }
}
