import Capacitor
import UIKit

@MainActor
enum NavigationItemStyle {
  static let font = UIFont.systemFont(ofSize: 12, weight: .medium)
}

// The web page owns navigation: history, the page stack, push and Back and
// their animation, and the edge swipe. UIKit draws chrome over it and reports
// what was tapped; it holds no navigation state of its own.
//
// Each tab is a navigation stack of one page, which hosts the one shared
// Capacitor controller while its tab is selected. The chrome follows the
// page's layout:
// - compact (iPhone): the tab bar, at the bottom; a page that declares its
//   header gets the navigation bar and a tab strip under it (`PageBar`);
// - compact on iPhone Duo: the tab bar stands on a side edge as the vertical
//   bar, and takes the page's Back and header actions (`PageBar`);
// - wide on iPad: no tab bar; the navigation bar is the window titlebar
//   (`Titlebar`) and the page shows its own rail;
// - wide on an open iPhone Duo: the vertical bar stays in place of the rail,
//   and takes the titlebar's search and sidebar toggle above its tabs; there
//   is no titlebar row.
//
// When the page on screen changes, its bars cross-fade as the page beneath
// them slides.
@MainActor
final class SilverhandNavigationController: UITabBarController, UITabBarControllerDelegate,
  UIGestureRecognizerDelegate
{
  let content = SilverhandViewController()
  let titlebar = Titlebar()
  let pageBar = PageBar()
  /// One per pane showing a conversation, by draft key.
  private(set) var composers: [String: ComposerView] = [:]
  var onComposerEvent: (([String: Any]) -> Void)?
  private var composerTopLimit: CGFloat = 0
  let banner = BannerView()
  let selectionBar = SelectionBarView()
  var onIntent: (([String: Any]) -> Void)?
  /// The selected page's safe area, horizontally: overlays that float over
  /// the page (composer, selection bar, banner) keep inside it. The root
  /// view's own safe area misses a vertical bar beside the page, as in Split
  /// View on iPhone Duo, where the bar stands on the app's outer edge.
  private let pageArea = UILayoutGuide()
  private var pageAreaSides: (left: NSLayoutConstraint, right: NSLayoutConstraint)?

  // Shared navigation state, as last synced from the page.
  private let names = ["chat", "activity", "tasks", "me"]
  private var session = ""
  private var entryKey = ""
  private var invalidatedSessions = Set<String>()
  private var pendingBadges: (session: String, values: [Bool])?
  private var modalVersion = 0
  private var blocked = false
  private var requestedVisible = false

  // Tab bar presence. A vertical bar stays on every page: it holds its place
  // beside the camera as the device folds, and never costs content height.
  private var barEdge: NSDirectionalRectEdge?
  private var visible: Bool { requestedVisible || verticalBarEdge != nil }
  private var appliedBars: AppliedBars?
  /// The bars as an edge swipe found them, fading with the finger.
  private var swiping: [UIView] = []
  /// The navigation bar's extent below the safe area, as last shown; the
  /// system bar's at first.
  private lazy var navigationBarHeight = UINavigationBar().sizeThatFits(view.bounds.size).height

  override func viewDidLoad() {
    super.viewDidLoad()
    delegate = self
    tabBar.tintColor = .label
    tabBar.unselectedItemTintColor = .label
    let appearance = tabBar.standardAppearance.copy()
    for layout in [appearance.stackedLayoutAppearance, appearance.inlineLayoutAppearance,
      appearance.compactInlineLayoutAppearance]
    {
      for state in [layout.normal, layout.selected] {
        state.titleTextAttributes = [.font: NavigationItemStyle.font, .foregroundColor: UIColor.label]
        state.iconColor = .label
      }
    }
    tabBar.standardAppearance = appearance
    tabBar.scrollEdgeAppearance = appearance
    // Asset names; the visible titles come from `NativeCopy`.
    let glyphs = ["Chats", "Activity", "Tasks", "Me"]
    let labels = NativeCopy.tabTitles
    // Each tab is a navigation stack of one page, whose navigation bar
    // carries the page's bars. Pages are pushed and popped in the web view.
    viewControllers = glyphs.enumerated().map { index, glyph in
      let stack = UINavigationController(rootViewController: PageHost())
      stack.setNavigationBarHidden(true, animated: false)
      stack.navigationBar.prefersLargeTitles = true
      stack.interactivePopGestureRecognizer?.isEnabled = false
      if #available(iOS 26.0, *) { stack.interactiveContentPopGestureRecognizer?.isEnabled = false }
      stack.tabBarItem = UITabBarItem(
        title: labels[index], image: UIImage(named: "Navigation" + glyph),
        selectedImage: UIImage(named: "Navigation" + glyph + "Selected"))
      stack.tabBarItem.tag = index
      return stack
    }
    if let first = page(0) { attachContent(to: first) }
    view.addLayoutGuide(pageArea)
    let left = pageArea.leftAnchor.constraint(equalTo: view.leftAnchor)
    let right = pageArea.rightAnchor.constraint(equalTo: view.rightAnchor)
    pageAreaSides = (left, right)
    NSLayoutConstraint.activate([
      left, right,
      pageArea.topAnchor.constraint(equalTo: view.topAnchor),
      pageArea.bottomAnchor.constraint(equalTo: view.bottomAnchor),
    ])
    hideBar(true, animated: false)
  }

  // MARK: Bars

  @available(iOS 27.1, *)
  override var childForPreferredVerticalBarBehavior: UIViewController? { nil }

  /// The theme is dark (from the navigation state).
  private var darkTheme = false
  /// The applied appearance: the shell's (the theme's) and the navigation
  /// bars' (also dark over the wide layout's dark window frame). Unset until
  /// first applied, so a light theme pins light under a dark system.
  private var appliedStyle: (shell: Bool, bars: Bool)?

  override var childForStatusBarStyle: UIViewController? { nil }
  override var preferredStatusBarStyle: UIStatusBarStyle {
    appliedStyle?.bars == true ? .lightContent : .darkContent
  }

  override var keyCommands: [UIKeyCommand]? {
    guard let state = titlebar.state, !blocked else { return nil }
    return [
      UIKeyCommand(
        title: state.searchShortcut, action: #selector(focusSearch), input: "k",
        modifierFlags: .command)
    ]
  }

  @objc private func focusSearch() { titlebar.focusSearch() }

  override func viewDidLayoutSubviews() {
    super.viewDidLayoutSubviews()
    if verticalBarEdge != barEdge {
      barEdge = verticalBarEdge
      if !session.isEmpty { hideBar(!visible || blocked, animated: false) }
    }
    applyBars()
    trackPageArea()
    for composer in composers.values { composer.publishMetrics() }
  }

  private func trackPageArea() {
    guard let sides = pageAreaSides, let pageView = page(selectedIndex)?.view else { return }
    let area = pageView.convert(pageView.bounds.inset(by: pageView.safeAreaInsets), to: view)
    let left = max(area.minX, view.safeAreaInsets.left)
    let right = max(view.bounds.maxX - area.maxX, view.safeAreaInsets.right)
    guard sides.left.constant != left || sides.right.constant != -right else { return }
    sides.left.constant = left
    sides.right.constant = -right
  }

  enum Bars: Equatable {
    case none
    case titlebar(TitlebarState)
    case page(PageBarState, vertical: Bool)
  }

  private struct AppliedBars: Equatable {
    let host: ObjectIdentifier
    let bars: Bars
    let width: CGFloat
    let enabled: Bool
  }

  /// What the selected page's navigation bar carries: the titlebar in the
  /// wide layout, else the page's declared header. The bar shows only while
  /// it carries one of them.
  private var bars: Bars {
    if let state = titlebar.state { return .titlebar(state) }
    if let state = pageBar.state { return .page(state, vertical: verticalBarEdge != nil) }
    return .none
  }

  func applyBars() {
    guard let stack = selectedViewController as? UINavigationController,
      let host = stack.topViewController
    else { return }
    let next = AppliedBars(
      host: ObjectIdentifier(host), bars: bars, width: view.bounds.width, enabled: !blocked)
    // Layout passes call this too; rebuilding unchanged items would restart
    // their glass animations.
    guard next != appliedBars else {
      place(next.bars, in: host, stack: stack)
      publishBarMetrics(in: stack)
      return
    }
    // Another page's bars cross-fade in: the bar appears, leaves, or names
    // another page. Changes within a page apply in place. This reads the bars
    // alone, which arrive apart from the navigation state.
    // One Tab root to another is a Tab switch, which swaps the page at once;
    // its bars do too (a fading glass button shows as a grey disc).
    let fade = appliedBars?.host == next.host && appliedBars.map {
      Self.page(of: $0.bars) != Self.page(of: next.bars)
        && !(Self.isTabRoot($0.bars) && Self.isTabRoot(next.bars))
    } == true && !UIAccessibility.isReduceMotionEnabled
    appliedBars = next
    defer { publishBarMetrics(in: stack) }
    // The old bars stay as images and fade out over the new ones. Glass bars
    // ignore their own alpha, so only images fade out reliably.
    let leaving = fade ? (swiping.isEmpty ? images(of: stack) : swiping) : []
    if !fade { endSwipe() }
    swiping = []
    draw(next.bars, on: host)
    applyChromeStyle()
    // A web modal's backdrop cannot cover native chrome, so the bars leave
    // while one is open.
    stack.setNavigationBarHidden(next.bars == .none || blocked, animated: false)
    place(next.bars, in: host, stack: stack)
    guard fade else { return }
    pageBar.accessory.setSwipeHidden(false)
    let arriving: [UIView] = [stack.navigationBar]
    for view in arriving { view.layer.opacity = 0 }
    UIView.animate(withDuration: 0.25, delay: 0, options: [.allowUserInteraction]) {
      for view in arriving { view.layer.opacity = 1 }
      for image in leaving { image.alpha = 0 }
    } completion: { _ in
      for view in arriving { view.layer.opacity = 1 }
      for image in leaving { image.removeFromSuperview() }
    }
  }

  /// An edge swipe carries the page `progress` of the way off: the bars fade
  /// with it, as an image over the bars, which hide while it lasts. Null ends
  /// the swipe with the bars back as they were.
  func setBackProgress(_ progress: CGFloat?) {
    guard let stack = selectedViewController as? UINavigationController else { return }
    for composer in composers.values { composer.setBackProgress(progress) }
    selectionBar.setBackProgress(progress)
    guard let progress else {
      endSwipe()
      return
    }
    if swiping.isEmpty {
      swiping = images(of: stack)
      stack.navigationBar.layer.opacity = 0
      pageBar.accessory.setSwipeHidden(true)
    }
    for image in swiping { image.alpha = 1 - min(1, max(0, progress)) }
  }

  private func endSwipe() {
    guard !swiping.isEmpty, let stack = selectedViewController as? UINavigationController else {
      return
    }
    for image in swiping { image.removeFromSuperview() }
    swiping = []
    stack.navigationBar.layer.opacity = 1
    pageBar.accessory.setSwipeHidden(false)
  }

  /// Images of the bars on screen, laid over them in the controller's view.
  private func images(of stack: UINavigationController) -> [UIView] {
    let shown = [stack.isNavigationBarHidden ? nil : stack.navigationBar as UIView,
      pageBar.accessory.window == nil ? nil : pageBar.accessory as UIView].compactMap { $0 }
    return shown.compactMap { bar in
      guard let image = bar.snapshotView(afterScreenUpdates: false) else { return nil }
      image.frame = bar.convert(bar.bounds, to: view)
      image.isUserInteractionEnabled = false
      view.insertSubview(image, belowSubview: tabBar)
      return image
    }
  }

  private static func isTabRoot(_ bars: Bars) -> Bool {
    if case .page(let state, _) = bars { return state.largeTitle && state.back == nil }
    return false
  }

  /// What a page's bars name it by, for telling one page's bars from another's.
  private static func page(of bars: Bars) -> String? {
    switch bars {
    case .none: nil
    case .titlebar: "titlebar"
    case .page(let state, _): "page:" + state.title + "\n" + (state.subtitle ?? "")
    }
  }

  /// Tells the page where its bars end. The bar keeps its height while
  /// hidden, so a page lays out for its bar before it shows.
  private func publishBarMetrics(in stack: UINavigationController) {
    let top = view.safeAreaInsets.top
    let bottom: CGFloat
    if verticalBarEdge != nil, titlebar.state != nil {
      // The panes' own 48pt headers clear the display's corners by their own
      // padding, so they need only a small gap above.
      bottom = view.safeAreaInsets.top + 10
    } else if verticalBarEdge != nil {
      bottom = verticalBarTop + PageAccessory.titleHeight
    } else if !stack.isNavigationBarHidden {
      bottom = stack.navigationBar.frame.maxY
      navigationBarHeight = stack.navigationBar.frame.height
    } else {
      bottom = top + navigationBarHeight
    }
    content.publishBarMetrics(
      bottom: bottom, tabs: PageAccessory.tabsHeight, vertical: verticalBarEdge != nil)
    composerTopLimit = bottom + (pageBar.accessory.window == nil ? 0 : pageBar.accessory.height)
    for composer in composers.values { composer.topLimit = composerTopLimit }
  }

  /// Draws `bars` into the page's navigation item. A page's bar keeps what
  /// it already shows (`PageBar.apply`).
  private func draw(_ bars: Bars, on page: UIViewController) {
    let item = page.navigationItem
    if case .page = bars {
      pageBar.apply(to: item, vertical: verticalBarEdge != nil, enabled: !blocked)
      return
    }
    pageBar.invalidate()
    item.titleView = nil
    item.title = nil
    item.leftBarButtonItems = nil
    item.rightBarButtonItems = nil
    if #available(iOS 16.0, *) { item.trailingItemGroups = [] }
    if #available(iOS 26.0, *) { item.subtitle = nil }
    item.largeTitleDisplayMode = .never
    if case .titlebar = bars {
      titlebar.apply(
        to: item, width: view.bounds.width, vertical: verticalBarEdge != nil, enabled: !blocked)
    }
  }

  /// The page's accessory (Duo title, view tabs) sits under the navigation
  /// bar; the page clears both itself (`publishBarMetrics`). With the vertical
  /// bar the navigation bar is empty, so the accessory starts at the top of
  /// the screen.
  private func place(_ bars: Bars, in page: UIViewController, stack: UINavigationController) {
    let accessory = pageBar.accessory
    guard case .page = bars, accessory.height > 0, !blocked else {
      accessory.removeFromSuperview()
      return
    }
    if accessory.superview !== page.view {
      page.view.addSubview(accessory)
      accessory.rebuildGlass()
    }
    let top = verticalBarEdge != nil ? verticalBarTop : stack.navigationBar.frame.maxY
    // Inside the page's side safe area, which holds a vertical bar on either
    // edge (in Split View, the app's outer one).
    let sides = page.view.safeAreaInsets
    accessory.frame = CGRect(
      x: sides.left, y: top, width: page.view.bounds.width - sides.left - sides.right,
      height: accessory.height)
  }

  // MARK: Pages

  /// The tab's page.
  private func page(_ index: Int) -> UIViewController? {
    (viewControllers?[index] as? UINavigationController)?.topViewController
  }

  private func attachContent(to host: UIViewController) {
    guard content.parent !== host else { return }
    content.willMove(toParent: nil)
    content.view.removeFromSuperview()
    content.removeFromParent()
    host.addChild(content)
    content.view.frame = host.view.bounds
    content.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
    host.view.insertSubview(content.view, at: 0)
    content.didMove(toParent: host)
  }

  /// The theme sets the whole shell's appearance, not the system's: the glass
  /// tab bar resolves its style above `tabBar`. The page paints itself from
  /// the theme and declares no color scheme, so the web view draws the same.
  /// The wide layout's dark window frame darkens the navigation bars alone.
  private func applyChromeStyle() {
    var dark = darkTheme
    // Beside the vertical bar the page is one canvas, without the dark frame.
    if case .titlebar(let state) = bars, verticalBarEdge == nil { dark = dark || state.dark }
    if let applied = appliedStyle, applied == (darkTheme, dark) { return }
    appliedStyle = (darkTheme, dark)
    overrideUserInterfaceStyle = darkTheme ? .dark : .light
    for case let other as UINavigationController in viewControllers ?? [] {
      other.navigationBar.overrideUserInterfaceStyle = dark ? .dark : .unspecified
    }
    setNeedsStatusBarAppearanceUpdate()
  }

  private func hideBar(_ hidden: Bool, animated: Bool) {
    if #available(iOS 18.0, *) {
      setTabBarHidden(hidden, animated: animated)
    } else {
      tabBar.isHidden = hidden
    }
  }

  // MARK: Shared navigation state

  func sync(_ call: CAPPluginCall) -> CGFloat {
    let nextSession = call.getString("session") ?? ""
    guard !nextSession.isEmpty, !invalidatedSessions.contains(nextSession) else { return 0 }
    if session != nextSession {
      session = nextSession
      for index in 0..<3 { setDot(false, at: index) }
    }
    applyBadges()
    let previousKey = entryKey
    let previousTab = selectedIndex
    entryKey = call.getString("key") ?? ""
    modalVersion = call.getInt("modalVersion") ?? 0
    blocked = call.getBool("blocked") ?? false
    requestedVisible = call.getBool("visible") ?? false
    darkTheme = call.getBool("dark") ?? false
    applyChromeStyle()
    if blocked {
      presentedViewController?.dismiss(animated: false)
      banner.dismiss()
    }
    for composer in composers.values { composer.setBlocked(blocked) }
    selectionBar.isHidden = blocked
    if let index = names.firstIndex(of: call.getString("tab") ?? ""), let host = page(index) {
      selectedIndex = index
      attachContent(to: host)
    }
    // A page pushed or popped within the tab takes the tab bar with it.
    let moved = selectedIndex == previousTab && entryKey != previousKey && !previousKey.isEmpty
    hideBar(!visible || blocked, animated: moved && !UIAccessibility.isReduceMotionEnabled)
    applyBars()
    view.layoutIfNeeded()
    observeContentScroll()
    return verticalBarEdge == nil ? tabBar.frame.height : 0
  }

  /// iPhone Duo stands the tab bar on a side edge. The page's side safe-area
  /// inset already clears it there, so no bottom inset applies.
  var verticalBarEdge: NSDirectionalRectEdge? { traitCollection.tabBarEdge }

  /// Where the page's own title starts beside the vertical bar. The top safe
  /// area there is zero, so the corner-adapted one keeps the title off the
  /// display's rounded corner.
  private var verticalBarTop: CGFloat {
    var top = view.safeAreaInsets.top
    if #available(iOS 26.0, *) {
      top = max(top, view.edgeInsets(for: .safeArea(cornerAdaptation: .vertical)).top)
    }
    return top + 4
  }

  // WebKit backs each overflow scroller with its own UIScrollView. Handing the
  // page's main one to UIKit lets the glass tab bar minimize on scroll. WebKit
  // may replace it after layout, so look again once the page settles.
  func observeContentScroll() {
    guard #available(iOS 26.0, *) else { return }
    tabBarMinimizeBehavior = visible ? .onScrollDown : .never
    let host = page(selectedIndex)
    for delay in [0.05, 0.6] {
      DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
        guard let self, let host, host === self.page(self.selectedIndex),
          let root = self.content.webView?.scrollView
        else { return }
        let scroller = self.mainScroller(in: root)
        host.setContentScrollView(self.visible ? scroller : nil, for: .bottom)
        // A declared header's bar dissolves the page's content as it scrolls
        // beneath it; the view tabs under the bar stay their own capsules,
        // outside the effect. A page's own backdrop or a Tab root's large
        // title lets the page show through instead, under the page's own
        // wash. The wide layout's titlebar has its own row above the page,
        // so nothing scrolls beneath it.
        let page = self.pageBar.state != nil
        let clear =
          self.titlebar.state != nil
          || self.pageBar.state.map { $0.backdrop || ($0.largeTitle && $0.back == nil) } ?? false
        host.setContentScrollView(page ? scroller : nil, for: .top)
        for view in [root, scroller].compactMap({ $0 }) { view.topEdgeEffect.isHidden = clear }
        self.pageBar.accessory.observe(page ? scroller : nil)
      }
    }
  }

  private func mainScroller(in root: UIScrollView) -> UIScrollView? {
    var best: UIScrollView?
    var bestArea: CGFloat = 0
    var stack: [UIView] = root.subviews
    while let next = stack.popLast() {
      stack.append(contentsOf: next.subviews)
      guard let scroller = next as? UIScrollView, scroller.window != nil,
        scroller.contentSize.height > scroller.bounds.height + 1
      else { continue }
      let area = scroller.bounds.width * scroller.bounds.height
      if area > bestArea {
        best = scroller
        bestArea = area
      }
    }
    return best
  }

  func invalidate() {
    if !session.isEmpty { invalidatedSessions.insert(session) }
    reset(session: session)
    // The page resets its bars whenever its layout changes, while the
    // composer is a page's own and leaves with it; only a lost session takes
    // it with the bars.
    removeComposer(key: nil)
    removeSelectionBar(key: nil)
    banner.dismiss()
  }

  // MARK: Tab items

  /// Redraws tab titles after the page reports its language.
  func applyLocale() {
    for (stack, title) in zip(viewControllers ?? [], NativeCopy.tabTitles) {
      stack.tabBarItem.title = title
    }
  }

  func badges(_ call: CAPPluginCall) {
    guard let owner = call.getString("session"), !invalidatedSessions.contains(owner) else {
      return
    }
    pendingBadges = (owner, names.prefix(3).map { call.getBool($0) == true })
    applyBadges()
  }

  private func applyBadges() {
    guard let pendingBadges, pendingBadges.session == session else { return }
    for (index, value) in pendingBadges.values.enumerated() {
      setDot(value, at: index)
    }
  }

  /// The system badge is a count capsule of at least 18pt even when empty;
  /// unread is an 8pt dot drawn into the tab image instead.
  private func setDot(_ shown: Bool, at index: Int) {
    guard let item = viewControllers?[index].tabBarItem else { return }
    let titles = ["Chats", "Activity", "Tasks"]
    guard index < titles.count else { return }
    let image = UIImage(named: "Navigation" + titles[index])
    let selected = UIImage(named: "Navigation" + titles[index] + "Selected")
    item.image = shown ? image.map(Self.dotted) : image
    item.selectedImage = shown ? selected.map(Self.dotted) : selected
  }

  /// Bakes the dot into the glyph's 24pt canvas, so the tab item keeps its size.
  /// The dot sits in the top-right corner with a 2pt knockout ring. Template
  /// tinting would paint the dot too, so each appearance is rendered in its
  /// resolved label colour and registered on one asset.
  private static func dotted(_ glyph: UIImage) -> UIImage {
    let size = CGSize(width: 24, height: 24)
    let center = CGPoint(x: size.width - 4, y: 4)
    let asset = UIImageAsset()
    for style in [UIUserInterfaceStyle.light, .dark] {
      let traits = UITraitCollection(userInterfaceStyle: style)
      let rendered = UIGraphicsImageRenderer(size: size).image { context in
        glyph.withTintColor(UIColor.label.resolvedColor(with: traits), renderingMode: .alwaysOriginal)
          .draw(in: CGRect(origin: .zero, size: size))
        let cg = context.cgContext
        cg.setBlendMode(.clear)
        cg.fillEllipse(in: CGRect(x: center.x - 6, y: center.y - 6, width: 12, height: 12))
        cg.setBlendMode(.normal)
        UIColor.systemRed.resolvedColor(with: traits).setFill()
        cg.fillEllipse(in: CGRect(x: center.x - 4, y: center.y - 4, width: 8, height: 8))
      }
      .withRenderingMode(.alwaysOriginal)
      // Without a display scale the asset hands back a 1x image of the
      // 3x bitmap, three times too large.
      asset.register(
        rendered,
        with: UITraitCollection(traitsFrom: [
          traits, UITraitCollection(displayScale: rendered.scale),
        ]))
    }
    return asset.image(with: UITraitCollection.current)
  }

  // MARK: Session

  func reset(session expected: String) {
    guard expected == session else { return }
    session = ""
    pendingBadges = nil
    applyBars()
    for index in 0..<3 { setDot(false, at: index) }
    entryKey = ""
    requestedVisible = false
    blocked = false
    hideBar(true, animated: false)
    presentedViewController?.dismiss(animated: false)
  }

  // MARK: Composer

  /// While the composer edits, a tap on the page only ends editing, as the
  /// tap that lowers a keyboard does elsewhere in iOS.
  private lazy var pageTap: UITapGestureRecognizer = {
    let tap = UITapGestureRecognizer(target: self, action: #selector(pageTapped))
    tap.delegate = self
    return tap
  }()

  private var editingComposer: ComposerView? { composers.values.first { $0.isEditing } }

  @objc private func pageTapped() { editingComposer?.endEditing() }

  func gestureRecognizerShouldBegin(_ recognizer: UIGestureRecognizer) -> Bool {
    recognizer !== pageTap
      || editingComposer?.endsEditing(at: recognizer.location(in: view)) == true
  }

  func gestureRecognizer(
    _ gestureRecognizer: UIGestureRecognizer,
    shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer
  ) -> Bool { true }

  /// Follows each touch on the page from its start to its end.
  private lazy var pageTouch: UILongPressGestureRecognizer = {
    let touch = UILongPressGestureRecognizer(target: self, action: #selector(pageTouched(_:)))
    touch.minimumPressDuration = 0
    touch.cancelsTouchesInView = false
    touch.delaysTouchesEnded = false
    touch.delegate = self
    return touch
  }()

  @objc private func pageTouched(_ touch: UILongPressGestureRecognizer) {
    switch touch.state {
    case .began: editingComposer?.holdFocus(true, at: touch.location(in: view))
    case .ended, .cancelled, .failed:
      for composer in composers.values { composer.holdFocus(false, at: .zero) }
    default: break
    }
  }

  func showBanner(_ notice: BannerNotice) {
    banner.show(notice, in: view, area: pageArea)
  }

  func showComposer(_ state: ComposerState) {
    if pageTap.view == nil, let web = content.webView {
      web.addGestureRecognizer(pageTap)
      web.addGestureRecognizer(pageTouch)
    }
    // A screen-wide composer is the page's only one: a new draft replaces it.
    if state.frame == nil {
      for (key, other) in composers where key != state.key {
        other.remove()
        composers[key] = nil
      }
    }
    let composer = composers[state.key] ?? makeComposer()
    composers[state.key] = composer
    composer.install(in: view, area: pageArea)
    composer.setBlocked(blocked)
    composer.setSteppedAside(selectionBar.state != nil)
    composer.apply(state)
  }

  private func makeComposer() -> ComposerView {
    let composer = ComposerView()
    composer.onEvent = { [weak self] event in self?.onComposerEvent?(event) }
    composer.presenter = self
    composer.topLimit = composerTopLimit
    return composer
  }

  /// Removes the composer for draft `key`; without a key, every one.
  func removeComposer(key: String?) {
    for (candidate, composer) in composers where key == nil || candidate == key {
      composer.remove()
      composers[candidate] = nil
    }
  }

  func setComposerText(key: String, text: String, caret: Int, focus: Bool) {
    composers[key]?.setText(key: key, text: text, caret: caret, focus: focus)
  }

  func showSelectionBar(_ state: SelectionBarState) {
    selectionBar.install(in: view, area: pageArea)
    selectionBar.isHidden = blocked
    selectionBar.apply(state)
    for composer in composers.values { composer.setSteppedAside(true) }
  }

  /// Removes the selection bar if it shows pane `key`, or whatever it shows
  /// without one; the composer comes back.
  func removeSelectionBar(key: String?) {
    guard selectionBar.state != nil, key == nil || selectionBar.state?.key == key else { return }
    selectionBar.remove()
    for composer in composers.values { composer.setSteppedAside(false) }
  }

  func tabBarController(
    _ tabBarController: UITabBarController, shouldSelect viewController: UIViewController
  ) -> Bool {
    guard !session.isEmpty, !blocked else { return false }
    let index = viewController.tabBarItem.tag
    onIntent?([
      "session": session, "modalVersion": modalVersion, "kind": "tab", "target": names[index],
    ])
    // Selection is acknowledged exclusively through shared state sync.
    return false
  }

  /// The Me tab shows the viewer's avatar; without one it keeps the asset glyph.
  func setAvatar(_ image: UIImage?) {
    guard let item = viewControllers?[3].tabBarItem else { return }
    guard let image else {
      item.image = UIImage(named: "NavigationMe")
      item.selectedImage = UIImage(named: "NavigationMeSelected")
      return
    }
    let avatar = Self.circularTabImage(image)
    item.image = avatar
    item.selectedImage = avatar
  }

  /// UIKit tints template images; an avatar keeps its own pixels. The glass
  /// selection platter alone marks the selected tab.
  private static func circularTabImage(_ image: UIImage) -> UIImage {
    let side: CGFloat = 26
    let bounds = CGRect(x: 0, y: 0, width: side, height: side)
    let rendered = UIGraphicsImageRenderer(size: bounds.size).image { _ in
      UIBezierPath(ovalIn: bounds).addClip()
      let scale = max(side / image.size.width, side / image.size.height)
      let size = CGSize(width: image.size.width * scale, height: image.size.height * scale)
      image.draw(
        in: CGRect(
          x: bounds.midX - size.width / 2, y: bounds.midY - size.height / 2,
          width: size.width, height: size.height))
    }
    return rendered.withRenderingMode(.alwaysOriginal)
  }
}

/// A tab's page: an empty host for the web content and its navigation item.
final class PageHost: UIViewController {
  init() {
    super.init(nibName: nil, bundle: nil)
    // Back is the page's own control; the system button never shows.
    navigationItem.hidesBackButton = true
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .systemBackground
  }
}
