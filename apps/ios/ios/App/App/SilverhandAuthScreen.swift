import SilverhandNative
import SwiftUI
import UIKit

// MARK: State

struct AuthScreenState {
  struct SceneMessage {
    var name, text: String
    var avatar: AvatarConfig
  }

  enum Connection {
    case checking(String)
    case failed(title: String, detail: String, technical: String, technicalLabel: String, retry: String?)
  }

  var dark: Bool
  var brand, headline, headlineAccent: String
  var channel, own: String
  var messages: [SceneMessage]
  var typist: AvatarConfig
  var logInLabel: String
  var createAccountLabel: String?
  var privacyLabel: String
  var privacyURL: URL?
  var serverURL, serverLabel: String?
  var connection: Connection?
  var enabled: Bool
  var sheet: AuthSheet?

  init?(_ raw: [String: Any]) {
    guard let scene = raw["scene"] as? [String: Any] else { return nil }
    dark = raw["dark"] as? Bool ?? false
    brand = raw["brand"] as? String ?? ""
    headline = raw["headline"] as? String ?? ""
    headlineAccent = raw["headlineAccent"] as? String ?? ""
    channel = scene["channel"] as? String ?? ""
    own = scene["own"] as? String ?? ""
    messages = (scene["messages"] as? [[String: Any]] ?? []).map {
      SceneMessage(
        name: $0["name"] as? String ?? "", text: $0["text"] as? String ?? "",
        avatar: Self.avatar($0["avatar"]))
    }
    typist = Self.avatar(scene["typist"])
    logInLabel = raw["logInLabel"] as? String ?? ""
    createAccountLabel = raw["createAccountLabel"] as? String
    let privacy = raw["privacy"] as? [String: Any]
    privacyLabel = privacy?["label"] as? String ?? ""
    privacyURL = (privacy?["url"] as? String).flatMap(URL.init(string:))
    let server = raw["server"] as? [String: Any]
    serverURL = server?["url"] as? String
    serverLabel = server?["label"] as? String
    if let connection = raw["connection"] as? [String: Any] {
      if connection["kind"] as? String == "checking" {
        self.connection = .checking(connection["label"] as? String ?? "")
      } else {
        self.connection = .failed(
          title: connection["title"] as? String ?? "",
          detail: connection["detail"] as? String ?? "",
          technical: connection["technical"] as? String ?? "",
          technicalLabel: connection["technicalLabel"] as? String ?? "",
          retry: connection["retryLabel"] as? String)
      }
    }
    enabled = raw["enabled"] as? Bool ?? false
    sheet = (raw["sheet"] as? [String: Any]).flatMap(AuthSheet.init)
  }

  static func avatar(_ raw: Any?) -> AvatarConfig {
    let raw = raw as? [String: Any] ?? [:]
    return AvatarConfig(
      parts: (raw["parts"] as? [String: Any] ?? [:]).compactMapValues { $0 as? Int },
      bg: raw["bg"] as? Int ?? 0)
  }
}

struct AuthSheet {
  enum Kind: String { case login, signup, recovery, server }
  enum Step: String { case email, reset, unavailable }

  var kind: Kind
  var title, closeLabel: String
  var raw: [String: Any]

  init?(_ raw: [String: Any]) {
    guard let kind = (raw["kind"] as? String).flatMap(Kind.init(rawValue:)) else { return nil }
    self.kind = kind
    title = raw["title"] as? String ?? ""
    closeLabel = raw["closeLabel"] as? String ?? ""
    self.raw = raw
  }

  func text(_ key: String) -> String { raw[key] as? String ?? "" }
  func optional(_ key: String) -> String? { raw[key] as? String }
  func flag(_ key: String) -> Bool { raw[key] as? Bool ?? false }

  var busy: Bool { flag("busy") }
  var error: String? { optional("error") }
  var step: Step { optional("step").flatMap(Step.init(rawValue:)) ?? .email }
  var minPasswordLength: Int { raw["minPasswordLength"] as? Int ?? 1 }
  var avatar: AvatarConfig? { raw["avatar"].map(AuthScreenState.avatar) }
}

// MARK: Presenter

/// The phone's signed-out screen: a welcome scene, with the login, signup,
/// recovery and server forms in sheets over it. The page keeps the session
/// calls, validation and every string, and declares the whole screen; this
/// draws it, keeps the fields and reports what was typed.
@MainActor
final class AuthScreenPresenter {
  var onEvent: (([String: Any]) -> Void)?

  private var screen: AuthWelcomeController?
  private var sheet: UIViewController?
  private var sheetKind: AuthSheet.Kind?
  private var model: AuthScreenModel?
  private lazy var library = try? AvatarLibrary.bundled()

  func show(_ state: AuthScreenState, from root: UIViewController) {
    let model = self.model ?? AuthScreenModel(library: library) { [weak self] event in
      self?.onEvent?(event)
    }
    self.model = model
    model.update(state)

    if screen?.presentingViewController == nil {
      guard root.presentedViewController == nil else { return }
      let screen = AuthWelcomeController(rootView: WelcomeView(model: model))
      screen.modalPresentationStyle = .overFullScreen
      screen.modalTransitionStyle = .crossDissolve
      screen.modalPresentationCapturesStatusBarAppearance = true
      // The keyboard belongs to the sheet; the scene behind it stays put.
      if #available(iOS 16.4, *) { screen.safeAreaRegions = .container }
      self.screen = screen
      sheet = nil
      sheetKind = nil
      root.present(screen, animated: true)
    }
    guard let screen else { return }
    screen.dark = state.dark
    sheet?.overrideUserInterfaceStyle = screen.overrideUserInterfaceStyle

    let wanted = state.sheet?.kind
    if wanted == sheetKind { return }
    let present = { [weak self] in
      guard let self, let wanted, self.sheetKind == wanted else { return }
      self.presentSheet(wanted, model: model, over: screen)
    }
    sheetKind = wanted
    if let open = sheet {
      sheet = nil
      open.dismiss(animated: true, completion: present)
    } else {
      present()
    }
  }

  func hide() {
    guard let screen else { return }
    self.screen = nil
    sheet = nil
    sheetKind = nil
    model = nil
    screen.presentingViewController?.dismiss(animated: true)
  }

  private func presentSheet(_ kind: AuthSheet.Kind, model: AuthScreenModel, over screen: UIViewController) {
    guard screen.presentedViewController == nil else { return }
    model.open(kind)
    let controller = UIHostingController(rootView: AuthSheetView(model: model))
    controller.overrideUserInterfaceStyle = screen.overrideUserInterfaceStyle
    controller.modalPresentationStyle = .pageSheet
    let dismissal = Dismissal { [weak self] in
      guard let self, self.sheet === controller else { return }
      self.sheet = nil
      self.sheetKind = nil
      self.onEvent?(["kind": "dismiss"])
    }
    controller.presentationController?.delegate = dismissal
    objc_setAssociatedObject(controller, &Self.dismissalKey, dismissal, .OBJC_ASSOCIATION_RETAIN)
    if let presentation = controller.sheetPresentationController {
      presentation.prefersGrabberVisible = true
      // Fitted to the form, so the scene behind stays in view.
      if #available(iOS 16.0, *) {
        presentation.detents = [
          .custom { [weak model] context in
            min(model?.sheetHeight ?? 320, context.maximumDetentValue)
          }
        ]
        model.onSheetHeight = { [weak presentation] in
          presentation?.animateChanges { presentation?.invalidateDetents() }
        }
      } else {
        presentation.detents = [kind == .signup ? .large() : .medium()]
      }
    }
    model.close = { [weak self, weak controller] in
      guard let self, let controller, self.sheet === controller else { return }
      self.sheet = nil
      self.sheetKind = nil
      controller.dismiss(animated: true)
      self.onEvent?(["kind": "dismiss"])
    }
    sheet = controller
    screen.present(controller, animated: true)
  }

  private static var dismissalKey = 0

  private final class Dismissal: NSObject, UIAdaptivePresentationControllerDelegate {
    let onDismiss: () -> Void
    init(onDismiss: @escaping () -> Void) { self.onDismiss = onDismiss }
    func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
      onDismiss()
    }
  }
}

/// Stays up across a navigation reset: it is what a signed-out page shows.
final class AuthWelcomeController: UIHostingController<WelcomeView> {
  /// The page's theme, which the screen and its sheets take over the system's.
  var dark = false {
    didSet {
      overrideUserInterfaceStyle = dark ? .dark : .light
      setNeedsStatusBarAppearanceUpdate()
    }
  }

  override var preferredStatusBarStyle: UIStatusBarStyle { dark ? .lightContent : .darkContent }
}

// MARK: Model

@MainActor
private final class AuthScreenModel: ObservableObject {
  @Published private(set) var state: AuthScreenState?
  @Published var email = ""
  @Published var handle = ""
  @Published var password = ""
  @Published var code = ""
  @Published var confirm = ""
  @Published var server = ""
  @Published var avatar: AvatarConfig?
  let library: AvatarLibrary?
  let send: ([String: Any]) -> Void
  var close: () -> Void = {}
  var onSheetHeight: () -> Void = {}
  private(set) var sheetHeight: CGFloat = 320
  /// The username follows the email's suggestion until the person types their own.
  private var handleEdited = false

  init(library: AvatarLibrary?, send: @escaping ([String: Any]) -> Void) {
    self.library = library
    self.send = send
  }

  var sheet: AuthSheet? { state?.sheet }

  func update(_ next: AuthScreenState) {
    if !handleEdited, let suggestion = next.sheet?.optional("handleSuggestion") { handle = suggestion }
    state = next
  }

  /// A sheet opens with what the page carries over; from there its fields are its own.
  func open(_ kind: AuthSheet.Kind) {
    guard let sheet else { return }
    password = ""
    code = ""
    confirm = ""
    handleEdited = false
    switch kind {
    case .login, .signup, .recovery:
      email = sheet.text("email")
      handle = sheet.text("handleSuggestion")
      avatar = sheet.avatar.flatMap { library?.normalized(parts: $0.parts, bg: $0.bg) }
    case .server:
      server = sheet.text("url")
    }
  }

  func measured(_ height: CGFloat) {
    guard abs(height - sheetHeight) > 0.5 else { return }
    sheetHeight = height
    onSheetHeight()
  }

  func emailChanged() {
    guard sheet?.kind == .login || sheet?.kind == .signup else { return }
    send(["kind": "input", "field": "email", "value": email])
  }

  /// Only a username the person typed is reported; the suggestion is the page's own.
  func handleTyped() {
    handleEdited = true
    send(["kind": "input", "field": "handle", "value": handle])
  }

  func shuffle() {
    guard let library else { return }
    let next = library.randomConfig()
    avatar = next
    send(["kind": "avatar", "config": ["parts": next.parts, "bg": next.bg]])
  }

  private func filled(_ text: String) -> Bool {
    !text.trimmingCharacters(in: .whitespaces).isEmpty
  }

  var canSubmit: Bool {
    guard let state, let sheet, state.enabled || sheet.kind == .server, !sheet.busy else {
      return false
    }
    switch sheet.kind {
    case .login:
      return filled(email) && password.count >= sheet.minPasswordLength
    case .signup:
      return filled(email) && password.count >= sheet.minPasswordLength && filled(handle)
        && !sheet.flag("handleInvalid")
    case .recovery:
      switch sheet.step {
      case .email: return filled(email)
      case .reset: return code.count == 6 && password.count >= 8 && !confirm.isEmpty
      case .unavailable: return false
      }
    case .server:
      return filled(server)
    }
  }

  func submit() {
    guard canSubmit, let sheet else { return }
    switch sheet.kind {
    case .login:
      send(["kind": "submit", "email": email, "password": password])
    case .signup:
      send(["kind": "submit", "email": email, "password": password, "handle": handle])
    case .recovery:
      if sheet.step == .email {
        send(["kind": "recoveryRequest", "email": email])
      } else {
        send(["kind": "recoveryReset", "code": code, "password": password, "confirm": confirm])
      }
    case .server:
      send(["kind": "serverSave", "url": server])
    }
  }
}

// MARK: Welcome

struct WelcomeView: View {
  @ObservedObject fileprivate var model: AuthScreenModel
  @Environment(\.accessibilityReduceMotion) private var reduceMotion
  @State private var shown = 0
  @State private var technical = false

  var body: some View {
    if let state = model.state {
      VStack(spacing: 0) {
        scene(state)
        panel(state)
      }
      .background(Color(UIColor.systemBackground).ignoresSafeArea())
      .onAppear {
        guard shown == 0 else { return }
        if reduceMotion {
          shown = 4
          return
        }
        for step in 1...4 {
          DispatchQueue.main.asyncAfter(deadline: .now() + 0.15 + Double(step - 1) * 0.25) {
            withAnimation(.spring(response: 0.55, dampingFraction: 0.8)) { shown = step }
          }
        }
      }
    }
  }

  // The scene takes whatever height the panel leaves; on a short screen it
  // loses its lower messages rather than pushing the actions down.
  private func scene(_ state: AuthScreenState) -> some View {
    ZStack(alignment: .topLeading) {
      IsoGrid()
        .mask(
          LinearGradient(
            stops: [
              .init(color: .clear, location: 0.04), .init(color: .black, location: 0.3),
              .init(color: .black, location: 0.6), .init(color: .clear, location: 1),
            ], startPoint: .top, endPoint: .bottom)
        )
        .ignoresSafeArea(edges: .top)

      VStack(alignment: .leading, spacing: 22) {
        (Text("#").foregroundColor(Color(UIColor.tertiaryLabel)) + Text(state.channel))
          .font(.system(size: 13, weight: .semibold))
          .foregroundColor(.secondary)
          .padding(.vertical, 5)
          .padding(.leading, 10)
          .padding(.trailing, 12)
          .background(Capsule().fill(Color(UIColor.secondarySystemBackground)))

        ForEach(Array(state.messages.enumerated()), id: \.offset) { index, message in
          HStack(alignment: .bottom, spacing: 8) {
            face(message.avatar)
            VStack(alignment: .leading, spacing: 2) {
              Text(message.name)
                .font(.system(size: 12, weight: .semibold))
                .foregroundColor(.secondary)
              Text(message.text).font(.system(size: 15))
            }
            .padding(.vertical, 9)
            .padding(.horizontal, 13)
            .background(Bubble(own: false).fill(Color(UIColor.secondarySystemBackground)))
          }
          .padding(.leading, index == 1 ? 32 : 0)
          .arriving(shown > index)
        }

        Text(state.own)
          .font(.system(size: 15))
          .foregroundColor(.white)
          .padding(.vertical, 9)
          .padding(.horizontal, 13)
          .background(Bubble(own: true).fill(Color(UIColor.systemBlue)))
          .frame(maxWidth: .infinity, alignment: .trailing)
          .arriving(shown > state.messages.count)

        HStack(alignment: .bottom, spacing: 8) {
          face(state.typist)
          TypingDots(still: reduceMotion)
            .padding(.vertical, 14)
            .padding(.horizontal, 14)
            .background(Bubble(own: false).fill(Color(UIColor.secondarySystemBackground)))
        }
        .arriving(shown > state.messages.count + 1)
      }
      .padding(.horizontal, 28)
      .padding(.top, 40)
    }
    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
    .clipped()
    .accessibilityHidden(true)
  }

  @ViewBuilder private func face(_ config: AvatarConfig) -> some View {
    if let library = model.library {
      let normalized = library.normalized(parts: config.parts, bg: config.bg)
      AvatarCanvas { context, side in library.draw(normalized, in: context, side: side) }
        .frame(width: 36, height: 36)
        .clipShape(RoundedRectangle(cornerRadius: 11, style: .continuous))
    }
  }

  private func panel(_ state: AuthScreenState) -> some View {
    VStack(alignment: .leading, spacing: 0) {
      HStack(spacing: 8) {
        Image("Brandmark")
          .renderingMode(.template)
          .resizable()
          .frame(width: 22, height: 22)
        Text(state.brand).font(.system(size: 15, weight: .semibold))
      }
      .foregroundColor(.secondary)
      .accessibilityElement(children: .combine)

      (Text(state.headline) + Text("\n")
        + Text(state.headlineAccent).foregroundColor(.secondary))
        .font(.system(size: 31, weight: .bold))
        .lineSpacing(2)
        .padding(.top, 18)
        .padding(.bottom, 32)
        .accessibilityAddTraits(.isHeader)

      // Ink rather than the accent: blue belongs to the viewer's own bubble above.
      VStack(spacing: 10) {
        action(state.logInLabel, primary: true, enabled: state.enabled) {
          model.send(["kind": "open", "sheet": "login"])
        }
        if let create = state.createAccountLabel {
          action(create, primary: false, enabled: state.enabled) {
            model.send(["kind": "open", "sheet": "signup"])
          }
        }
      }

      if let connection = state.connection { status(connection) }

      HStack(spacing: 16) {
        if let url = state.privacyURL {
          Button(state.privacyLabel) { UIApplication.shared.open(url) }
        }
        Spacer(minLength: 0)
        if let server = state.serverURL {
          Button("\(server) \u{203A}") { model.send(["kind": "open", "sheet": "server"]) }
            .accessibilityLabel(state.serverLabel ?? server)
        }
      }
      .font(.footnote)
      .foregroundColor(.secondary)
      .lineLimit(1)
      .frame(minHeight: 32)
      .padding(.top, 22)
    }
    .padding(.horizontal, 28)
    .padding(.top, 16)
    .padding(.bottom, 12)
  }

  private func action(_ label: String, primary: Bool, enabled: Bool, run: @escaping () -> Void)
    -> some View
  {
    Button(action: run) {
      Text(label)
        .font(.system(size: 17, weight: .semibold))
        .frame(maxWidth: .infinity, minHeight: 54)
        .foregroundColor(primary ? Color(UIColor.systemBackground) : .primary)
        .background(
          RoundedRectangle(cornerRadius: 16, style: .continuous)
            .fill(primary ? Color(UIColor.label) : Color(UIColor.tertiarySystemFill)))
    }
    .buttonStyle(PressStyle())
    .disabled(!enabled)
    .opacity(enabled ? 1 : 0.4)
  }

  @ViewBuilder private func status(_ connection: AuthScreenState.Connection) -> some View {
    switch connection {
    case .checking(let label):
      HStack(spacing: 8) {
        ProgressView().scaleEffect(0.8)
        Text(label)
      }
      .font(.footnote)
      .foregroundColor(.secondary)
      .padding(.top, 14)
    case .failed(let title, let detail, let details, let technicalLabel, let retry):
      VStack(alignment: .leading, spacing: 8) {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
          Image(systemName: "exclamationmark.circle.fill").foregroundColor(.red)
          (Text(title).fontWeight(.semibold).foregroundColor(.primary) + Text(" ") + Text(detail))
            .foregroundColor(.secondary)
        }
        HStack {
          Button {
            withAnimation { technical.toggle() }
          } label: {
            HStack(spacing: 4) {
              Text(technicalLabel)
              Image(systemName: technical ? "chevron.up" : "chevron.down")
                .font(.system(size: 10, weight: .semibold))
            }
          }
          .foregroundColor(.secondary)
          Spacer()
          if let retry {
            Button(retry) { model.send(["kind": "retry"]) }
              .font(.footnote.weight(.semibold))
              .foregroundColor(.primary)
          }
        }
        if technical {
          Text(details).foregroundColor(.secondary).textSelection(.enabled)
        }
      }
      .font(.footnote)
      .padding(.top, 14)
    }
  }
}

private struct PressStyle: ButtonStyle {
  func makeBody(configuration: Configuration) -> some View {
    configuration.label.opacity(configuration.isPressed ? 0.7 : 1)
  }
}

private struct ArrivalModifier: ViewModifier {
  let shown: Bool
  func body(content: Content) -> some View {
    content
      .opacity(shown ? 1 : 0)
      .offset(y: shown ? 0 : 12)
      .scaleEffect(shown ? 1 : 0.98)
  }
}

extension View {
  fileprivate func arriving(_ shown: Bool) -> some View { modifier(ArrivalModifier(shown: shown)) }
}

/// A chat bubble with the corner nearest its sender tucked in.
private struct Bubble: Shape {
  let own: Bool

  func path(in rect: CGRect) -> Path {
    let big: CGFloat = 18
    let small: CGFloat = 6
    let bottomLeft = own ? big : small
    let bottomRight = own ? small : big
    var path = Path()
    path.move(to: CGPoint(x: rect.minX + big, y: rect.minY))
    path.addLine(to: CGPoint(x: rect.maxX - big, y: rect.minY))
    path.addArc(
      tangent1End: CGPoint(x: rect.maxX, y: rect.minY),
      tangent2End: CGPoint(x: rect.maxX, y: rect.maxY), radius: big)
    path.addArc(
      tangent1End: CGPoint(x: rect.maxX, y: rect.maxY),
      tangent2End: CGPoint(x: rect.minX, y: rect.maxY), radius: bottomRight)
    path.addArc(
      tangent1End: CGPoint(x: rect.minX, y: rect.maxY),
      tangent2End: CGPoint(x: rect.minX, y: rect.minY), radius: bottomLeft)
    path.addArc(
      tangent1End: CGPoint(x: rect.minX, y: rect.minY),
      tangent2End: CGPoint(x: rect.maxX, y: rect.minY), radius: big)
    path.closeSubpath()
    return path
  }
}

private struct TypingDots: View {
  let still: Bool

  var body: some View {
    if still {
      dots(at: 0)
    } else {
      TimelineView(.animation) { context in
        dots(at: context.date.timeIntervalSinceReferenceDate)
      }
    }
  }

  private func dots(at time: TimeInterval) -> some View {
    HStack(spacing: 4) {
      ForEach(0..<3) { index in
        Circle()
          .fill(Color.secondary)
          .frame(width: 7, height: 7)
          .opacity(still ? 0.6 : opacity(time - Double(index) * 0.15))
      }
    }
  }

  /// Each dot brightens once per 1.2s cycle, in the first half of it.
  private func opacity(_ time: TimeInterval) -> Double {
    let phase = time.truncatingRemainder(dividingBy: 1.2) / 1.2
    let rise = phase < 0.3 ? phase / 0.3 : phase < 0.6 ? (0.6 - phase) / 0.3 : 0
    return 0.35 + 0.65 * max(0, rise)
  }
}

/// The brand's isometric grid: two line families at 30° and 150°, 88pt apart.
private struct IsoGrid: View {
  var body: some View {
    Canvas { context, size in
      let spacing: CGFloat = 88
      let extent = size.width + size.height
      for angle in [CGFloat.pi / 6, CGFloat.pi * 5 / 6] {
        // Lines run perpendicular to the direction the stripes step along.
        let step = CGVector(dx: sin(angle), dy: -cos(angle))
        let run = CGVector(dx: -step.dy, dy: step.dx)
        var path = Path()
        var offset = -extent
        while offset <= extent {
          let center = CGPoint(x: step.dx * offset, y: size.height + step.dy * offset)
          path.move(to: CGPoint(x: center.x - run.dx * extent, y: center.y - run.dy * extent))
          path.addLine(to: CGPoint(x: center.x + run.dx * extent, y: center.y + run.dy * extent))
          offset += spacing
        }
        context.stroke(path, with: .color(Color(UIColor.separator).opacity(0.5)), lineWidth: 1)
      }
    }
  }
}

// MARK: Sheets

private struct AuthSheetView: View {
  @ObservedObject var model: AuthScreenModel
  @FocusState private var focus: Field?

  private enum Field: Hashable { case email, handle, password, code, confirm, server }

  var body: some View {
    ScrollView {
      if let sheet = model.sheet {
        VStack(alignment: .leading, spacing: 10) {
          header(sheet)
          content(sheet)
        }
        .padding(.horizontal, 22)
        .padding(.top, 22)
        .padding(.bottom, 16)
        .background(
          GeometryReader { proxy in
            Color.clear.preference(key: HeightKey.self, value: proxy.size.height)
          })
      }
    }
    .onPreferenceChange(HeightKey.self) { model.measured($0) }
    .onAppear {
      // The sheet's own animation runs first; focusing at once drops the keyboard in mid-slide.
      DispatchQueue.main.asyncAfter(deadline: .now() + 0.45) {
        guard let sheet = model.sheet else { return }
        switch sheet.kind {
        case .login: focus = .email
        case .signup: focus = model.library == nil ? .email : nil
        case .recovery: focus = sheet.step == .email ? .email : .code
        case .server: focus = .server
        }
      }
    }
  }

  private func header(_ sheet: AuthSheet) -> some View {
    HStack {
      Text(sheet.title).font(.title2.weight(.bold))
      Spacer()
      Button(action: model.close) {
        Image(systemName: "xmark")
          .font(.system(size: 13, weight: .semibold))
          .foregroundColor(.secondary)
          .frame(width: 30, height: 30)
          .background(Circle().fill(Color(UIColor.tertiarySystemFill)))
      }
      .accessibilityLabel(sheet.closeLabel)
    }
    .padding(.bottom, 8)
  }

  @ViewBuilder private func content(_ sheet: AuthSheet) -> some View {
    switch sheet.kind {
    case .login, .signup: account(sheet)
    case .recovery: recovery(sheet)
    case .server: server(sheet)
    }
  }

  @ViewBuilder private func account(_ sheet: AuthSheet) -> some View {
    let signup = sheet.kind == .signup
    if signup { avatar(sheet) }

    field(
      sheet.text("emailPlaceholder"), text: $model.email, focus: .email, content: .username,
      keyboard: .emailAddress, next: signup ? .handle : .password
    )
    .onChange(of: model.email) { _ in model.emailChanged() }

    if signup {
      VStack(alignment: .leading, spacing: 6) {
        field(
          sheet.text("handlePlaceholder"), text: $model.handle, focus: .handle,
          content: .nickname, next: .password
        )
        .onChange(of: model.handle) { _ in
          if focus == .handle { model.handleTyped() }
        }
        if let handleNote = sheet.optional("handleNote") {
          note(handleNote, error: sheet.flag("handleInvalid"))
        }
      }
    }

    ZStack(alignment: .trailing) {
      field(
        sheet.text("passwordPlaceholder"), text: $model.password, focus: .password,
        content: signup ? .newPassword : .password, secure: true)
      if let forgot = sheet.optional("forgotLabel") {
        Button(forgot) { model.send(["kind": "forgot", "email": model.email]) }
          .font(.subheadline)
          .foregroundColor(.secondary)
          .padding(.trailing, 16)
      }
    }

    if let message = sheet.error ?? sheet.optional("notice") {
      note(message, error: sheet.error != nil)
    }

    submit(sheet.text("submitLabel"), busy: sheet.busy)
  }

  @ViewBuilder private func recovery(_ sheet: AuthSheet) -> some View {
    if let message = sheet.optional("message") {
      Text(message)
        .font(.subheadline)
        .foregroundColor(.secondary)
        .fixedSize(horizontal: false, vertical: true)
        .padding(.horizontal, 4)
        .padding(.bottom, 4)
    }

    switch sheet.step {
    case .email:
      field(
        sheet.text("emailPlaceholder"), text: $model.email, focus: .email, content: .username,
        keyboard: .emailAddress)
      if let error = sheet.error { note(error, error: true) }
      submit(sheet.text("sendLabel"), busy: sheet.busy)
    case .reset:
      field(
        sheet.text("codePlaceholder"), text: $model.code, focus: .code, content: .oneTimeCode,
        keyboard: .numberPad, next: .password
      )
      .onChange(of: model.code) { value in
        let digits = String(value.filter(\.isNumber).prefix(6))
        if digits != value { model.code = digits }
      }
      field(
        sheet.text("passwordPlaceholder"), text: $model.password, focus: .password,
        content: .newPassword, secure: true, next: .confirm)
      field(
        sheet.text("confirmPlaceholder"), text: $model.confirm, focus: .confirm,
        content: .newPassword, secure: true)
      if let error = sheet.error { note(error, error: true) }
      submit(sheet.text("resetLabel"), busy: sheet.busy)
    case .unavailable:
      EmptyView()
    }

    HStack(spacing: 20) {
      if sheet.step == .reset {
        Button(sheet.text("resendLabel")) {
          model.send(["kind": "recoveryRequest", "email": model.email])
        }
        .disabled(sheet.busy)
      }
      Button(sheet.text("backLabel")) { model.send(["kind": "recoveryBack"]) }
    }
    .font(.subheadline.weight(.medium))
    .foregroundColor(.secondary)
    .frame(maxWidth: .infinity)
    .padding(.top, 6)
  }

  @ViewBuilder private func server(_ sheet: AuthSheet) -> some View {
    field(
      sheet.text("placeholder"), text: $model.server, focus: .server, content: .URL,
      keyboard: .URL)
    if let error = sheet.error { note(error, error: true) }
    submit(sheet.text("saveLabel"), busy: sheet.busy)
  }

  private func note(_ text: String, error: Bool) -> some View {
    Text(text)
      .font(.footnote)
      .foregroundColor(error ? .red : .secondary)
      .fixedSize(horizontal: false, vertical: true)
      .padding(.horizontal, 4)
  }

  private func submit(_ label: String, busy: Bool) -> some View {
    let active = model.canSubmit || busy
    return Button(action: model.submit) {
      ZStack {
        if busy {
          ProgressView().tint(Color(UIColor.systemBackground))
        } else {
          Text(label).font(.headline)
        }
      }
      .frame(maxWidth: .infinity, minHeight: 54)
      .foregroundColor(active ? Color(UIColor.systemBackground) : .secondary)
      .background(
        RoundedRectangle(cornerRadius: 16, style: .continuous)
          .fill(active ? Color(UIColor.label) : Color(UIColor.tertiarySystemFill)))
    }
    .buttonStyle(.plain)
    .disabled(!model.canSubmit)
    .padding(.top, 6)
  }

  @ViewBuilder private func avatar(_ sheet: AuthSheet) -> some View {
    if let library = model.library, let config = model.avatar {
      VStack(spacing: 10) {
        Button(action: model.shuffle) {
          ZStack(alignment: .bottomTrailing) {
            AvatarCanvas { context, side in library.draw(config, in: context, side: side) }
              .frame(width: 96, height: 96)
              .clipShape(RoundedRectangle(cornerRadius: 28, style: .continuous))
              .shadow(color: .black.opacity(0.12), radius: 12, y: 6)
            Image(systemName: "shuffle")
              .font(.system(size: 13, weight: .semibold))
              .foregroundColor(.primary)
              .frame(width: 32, height: 32)
              .background(Circle().fill(Color(UIColor.systemBackground)))
              .shadow(color: .black.opacity(0.15), radius: 4, y: 2)
              .offset(x: 6, y: 6)
          }
        }
        .buttonStyle(.plain)
        .accessibilityLabel(NativeCopy.random)
        if let caption = sheet.optional("avatarCaption") {
          Text(caption).font(.footnote).foregroundColor(.secondary)
        }
      }
      .frame(maxWidth: .infinity)
      .padding(.bottom, 12)
    }
  }

  private func field(
    _ placeholder: String, text: Binding<String>, focus field: Field,
    content: UITextContentType, keyboard: UIKeyboardType = .default, secure: Bool = false,
    next: Field? = nil
  ) -> some View {
    Group {
      if secure {
        SecureField(placeholder, text: text)
      } else {
        TextField(placeholder, text: text)
          .keyboardType(keyboard)
      }
    }
    .textContentType(content)
    .textInputAutocapitalization(.never)
    .disableAutocorrection(true)
    .focused($focus, equals: field)
    .submitLabel(next == nil ? .go : .next)
    .onSubmit {
      if let next {
        focus = next
      } else {
        model.submit()
      }
    }
    .font(.body)
    .padding(.horizontal, 16)
    .frame(height: 52)
    .background(
      RoundedRectangle(cornerRadius: 14, style: .continuous)
        .fill(Color(UIColor.tertiarySystemFill)))
  }
}

private struct HeightKey: PreferenceKey {
  static var defaultValue: CGFloat = 0
  static func reduce(value: inout CGFloat, nextValue: () -> CGFloat) { value = max(value, nextValue()) }
}
