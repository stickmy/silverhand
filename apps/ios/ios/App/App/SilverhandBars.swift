import UIKit

// The selected page's navigation bar carries one of three bars, all owned by
// the web page: the wide layout's titlebar, a compact page's own bar, or the
// page controls iPhone Duo lends to its vertical bar.
//
// The contract: the page declares each bar as data (every state, rule and
// string), the plugin decodes it, these types render it, and a tap goes back
// to the page as an event naming what was tapped. Nothing here decides
// product behaviour or holds product copy.

/// A page control drawn as a bar item.
struct BarAction: Equatable {
  let id, label: String
  /// Spoken in place of `label`; nil uses `label`.
  var accessibilityLabel: String? = nil
  /// PNG of the web control's glyph; drawn as a template so it takes the bar's ink.
  let icon: String
  let selected, dot: Bool
  var destructive = false
  /// The page's main action; a bar keeps it out of the overflow menu.
  var primary = false
}

/// Web glyphs as template images, decoded once.
@MainActor
enum BarGlyphs {
  private static var images: [String: UIImage] = [:]

  /// The glyph at `size` points; the page sends it at 20.
  static func image(_ base64: String, size: CGFloat = 20) -> UIImage? {
    let key = "\(size):\(base64)"
    if let cached = images[key] { return cached }
    guard let data = Data(base64Encoded: base64), var image = UIImage(data: data, scale: 3) else {
      return nil
    }
    if size != 20 {
      let bounds = CGRect(x: 0, y: 0, width: size, height: size)
      image = UIGraphicsImageRenderer(bounds: bounds).image { _ in image.draw(in: bounds) }
    }
    image = image.withRenderingMode(.alwaysTemplate)
    if images.count > 96 { images.removeAll() }
    images[key] = image
    return image
  }

  static func item(for action: BarAction, onTap: @escaping (String) -> Void) -> UIBarButtonItem {
    let item = UIBarButtonItem(
      image: image(action.icon), primaryAction: UIAction { _ in onTap(action.id) })
    // The bar draws the image; the title is what the system's overflow menu
    // lists when a crowded bar folds items into it.
    item.title = action.label
    item.accessibilityLabel = action.accessibilityLabel ?? action.label
    item.isSelected = action.selected
    // The system selection, in ink: the accent colour is the brand's, not a
    // toggle state.
    item.tintColor = .label
    if action.dot, #available(iOS 26.0, *) { item.badge = .indicator() }
    return item
  }

  /// One fixed group per web capsule: the system draws each as one glass capsule.
  @available(iOS 16.0, *)
  static func groups(_ items: [[UIBarButtonItem]]) -> [UIBarButtonItemGroup] {
    items.map { UIBarButtonItemGroup.fixedGroup(representativeItem: nil, items: $0) }
  }
}

struct PageBarTab: Equatable {
  let item: BarAction
  /// The menu item that closes the tab; nil when it cannot close.
  let closeLabel: String?
}

struct PageBarAddMenu: Equatable {
  let label: String
  let items: [BarAction]
}

/// The identity a tappable title wears, as PNG: a glyph drawn as a template,
/// or a picture drawn as it is.
enum PageBarTitleIcon: Equatable {
  case glyph(String)
  case image(String)
}

struct PageBarState: Equatable {
  let title: String
  let subtitle: String?
  /// The title's accessibility hint; nil when the title is not a button.
  let titleAction: String?
  var titleIcon: PageBarTitleIcon? = nil
  /// The page paints a backdrop under the bar, shown in place of the edge fade.
  var backdrop = false
  /// A Tab root's title shows large.
  let largeTitle: Bool
  /// Back's label; nil hides Back.
  let back: String?
  let actions: [[BarAction]]
  /// The button a horizontal bar gathers the actions behind, as a menu.
  var overflow: BarAction? = nil
  /// View tabs under the bar; nil shows none.
  var tabs: [PageBarTab]?
  var addTabs: PageBarAddMenu?
}

/// A compact page's header: Back, the title and the page's action capsules
/// in the system navigation bar, with the page's view tabs as glass capsules
/// in a strip under it.
///
/// On iPhone Duo, Back and the actions move into the vertical bar. The
/// navigation bar then carries nothing and takes no height, and the title
/// joins the tabs in the strip, which starts at the top of the screen as
/// Safari's page does.
@MainActor
final class PageBar {
  var state: PageBarState?
  var onEvent: (([String: Any]) -> Void)?
  let accessory = PageAccessory()
  private lazy var titleButton: UIButton = {
    let button = UIButton(configuration: .plain())
    button.addAction(
      UIAction { [weak self] _ in self?.onEvent?(["kind": "title"]) }, for: .primaryActionTriggered)
    return button
  }()

  /// The header last drawn into a navigation item, without the view tabs.
  private struct Header: Equatable {
    let item: ObjectIdentifier
    let state: PageBarState
    let vertical, enabled: Bool
  }
  private var drawn: Header?

  init() {
    accessory.onEvent = { [weak self] in self?.onEvent?($0) }
  }

  private func header(
    for state: PageBarState, item: UINavigationItem, vertical: Bool, enabled: Bool
  ) -> Header {
    // Every other field is part of the header, including fields added later.
    var header = state
    header.tabs = nil
    header.addTabs = nil
    return Header(item: ObjectIdentifier(item), state: header, vertical: vertical, enabled: enabled)
  }

  /// Forgets the drawn header, once something else has drawn into its item.
  func invalidate() { drawn = nil }

  func apply(to item: UINavigationItem, vertical: Bool, enabled: Bool) {
    guard let state else { return }
    // A change of view tab leaves the header as it is; redrawing its title
    // and buttons would make them blink.
    let next = header(for: state, item: item, vertical: vertical, enabled: enabled)
    guard next != drawn else {
      updateTabs(state: state, vertical: vertical, enabled: enabled)
      return
    }
    drawn = next
    item.titleView = nil
    item.title = nil
    item.leftBarButtonItems = nil
    item.rightBarButtonItems = nil
    if #available(iOS 16.0, *) { item.trailingItemGroups = [] }
    if #available(iOS 26.0, *) { item.subtitle = nil }
    let send: (String) -> Void = { [weak self] id in
      self?.onEvent?(id == "back" ? ["kind": "back"] : ["kind": "action", "id": id])
    }
    let backItem = state.back.map {
      BarGlyphs.item(
        for: BarAction(id: "back", label: $0, icon: "", selected: false, dot: false), onTap: send)
    }
    backItem?.image = UIImage(systemName: "chevron.backward")
    let actions = state.actions.map { $0.map { BarGlyphs.item(for: $0, onTap: send) } }
    for item in actions.joined() { item.isEnabled = enabled }
    backItem?.isEnabled = enabled
    // A Tab root's large title shares the row with the bar's buttons, so the
    // bar keeps one height on every page; Duo's vertical bar and iOS before 17
    // show it small.
    if #available(iOS 17.0, *), state.largeTitle, !vertical {
      item.largeTitleDisplayMode = .inline
    } else {
      item.largeTitleDisplayMode = .never
    }
    // A header stays put while the page scrolls.
    if #available(iOS 27.0, *) {
      var minimization = item.navigationBarMinimization
      minimization.minimizationBehavior = .never
      item.navigationBarMinimization = minimization
    }

    if vertical, #available(iOS 27.1, *) {
      let groups = (backItem.map { [[$0]] } ?? []) + actions
      for item in groups.joined() { item.axisBehavior = .verticalPreferred }
      item.verticalBarCompressionBehavior = .prefersTabBar
      item.trailingItemGroups = BarGlyphs.groups(groups)
      if state.titleAction != nil {
        style(titleButton, for: state, alignment: .leading, enabled: enabled)
      }
      updateTabs(state: state, vertical: true, enabled: enabled)
      return
    }

    item.leftBarButtonItems = backItem.map { [$0] }
    // In a horizontal bar the selection disc takes the item's tint and so
    // would hide a template glyph; the glyph is inverted here.
    for action in actions.joined() where action.isSelected {
      action.image = action.image?.withTintColor(.systemBackground, renderingMode: .alwaysOriginal)
    }
    // The overflow menu gathers every action but the primary ones, which stay
    // in the bar ahead of it.
    let trailing =
      state.overflow.map { more in
        let pinned = zip(state.actions, actions).map { declared, items in
          zip(declared, items).filter { $0.0.primary }.map(\.1)
        }.filter { !$0.isEmpty }
        let gathered = state.actions.map { $0.filter { !$0.primary } }.filter { !$0.isEmpty }
        return pinned + [[menuItem(more, actions: gathered, enabled: enabled, onTap: send)]]
      } ?? actions
    if #available(iOS 16.0, *) {
      item.trailingItemGroups = BarGlyphs.groups(trailing)
    } else {
      item.rightBarButtonItems = trailing.flatMap { $0 }.reversed()
    }
    if state.titleAction != nil {
      style(titleButton, for: state, alignment: .center, enabled: enabled)
      // At its own size, so the bar centres it as it does a plain title.
      titleButton.sizeToFit()
      item.titleView = titleButton
    } else {
      item.title = state.title
      if #available(iOS 26.0, *) { item.subtitle = state.subtitle }
    }
    updateTabs(state: state, vertical: false, enabled: enabled)
  }

  /// One button opening the actions as a menu, a selected action checked; it
  /// carries the unread dot of any action in it.
  private func menuItem(
    _ more: BarAction, actions: [[BarAction]], enabled: Bool, onTap: @escaping (String) -> Void
  ) -> UIBarButtonItem {
    let sections = actions.map { group in
      UIMenu(
        options: .displayInline,
        children: group.map { action in
          UIAction(
            title: action.label, image: BarGlyphs.image(action.icon, size: 18),
            attributes: action.destructive ? .destructive : [],
            state: action.selected ? .on : .off
          ) { _ in onTap(action.id) }
        })
    }
    let dot = actions.joined().contains { $0.dot }
    let item = BarGlyphs.item(
      for: BarAction(id: more.id, label: more.label, icon: more.icon, selected: false, dot: dot),
      onTap: onTap)
    item.primaryAction = nil
    item.menu = UIMenu(children: sections)
    item.isEnabled = enabled
    return item
  }

  /// The strip under the bar: the view tabs, and on iPhone Duo the title.
  private func updateTabs(state: PageBarState, vertical: Bool, enabled: Bool) {
    let duo = vertical && state.titleAction == nil
    let button = vertical && state.titleAction != nil
    accessory.update(
      title: duo ? PageAccessory.Title(state: state) : nil, titleButton: button ? titleButton : nil,
      tabs: state.tabs, add: state.addTabs, enabled: enabled)
  }

  private static let titlePicturePoints: CGFloat = 30

  /// A glyph at the bar's glyph size, or a picture as a circle, concentric with the capsule's end.
  private static func titleImage(_ icon: PageBarTitleIcon) -> UIImage? {
    switch icon {
    case .glyph(let png):
      // Centred in the picture's square, so every title capsule is one height.
      guard let glyph = BarGlyphs.image(png) else { return nil }
      let bounds = CGRect(x: 0, y: 0, width: titlePicturePoints, height: titlePicturePoints)
      let inset = (titlePicturePoints - glyph.size.width) / 2
      return UIGraphicsImageRenderer(bounds: bounds).image { _ in
        glyph.draw(in: bounds.insetBy(dx: inset, dy: inset))
      }.withRenderingMode(.alwaysTemplate)
    case .image(let png):
      guard let data = Data(base64Encoded: png), let picture = UIImage(data: data, scale: 3) else {
        return nil
      }
      let bounds = CGRect(x: 0, y: 0, width: titlePicturePoints, height: titlePicturePoints)
      return UIGraphicsImageRenderer(bounds: bounds).image { _ in
        UIBezierPath(ovalIn: bounds).addClip()
        picture.draw(in: bounds)
      }.withRenderingMode(.alwaysOriginal)
    }
  }

  private func style(
    _ button: UIButton, for state: PageBarState, alignment: UIButton.Configuration.TitleAlignment,
    enabled: Bool
  ) {
    // A title with an identity is a glass capsule, as the bar's buttons are.
    var configuration = UIButton.Configuration.plain()
    if #available(iOS 26.0, *), let icon = state.titleIcon {
      configuration = .glass()
      configuration.image = Self.titleImage(icon)
      configuration.imagePadding = 8
      // An even margin round the picture keeps it concentric with the capsule's end.
      configuration.contentInsets = NSDirectionalEdgeInsets(
        top: 6, leading: 6, bottom: 6, trailing: 16)
    }
    configuration.baseForegroundColor = .label
    configuration.titleAlignment = alignment
    configuration.attributedTitle = AttributedString(
      state.title,
      attributes: AttributeContainer([.font: UIFont.preferredFont(forTextStyle: .headline)]))
    if let subtitle = state.subtitle {
      configuration.attributedSubtitle = AttributedString(
        subtitle,
        attributes: AttributeContainer([
          .font: UIFont.preferredFont(forTextStyle: .caption1),
          .foregroundColor: UIColor.secondaryLabel,
        ]))
    }
    configuration.titleLineBreakMode = .byTruncatingTail
    if alignment == .leading, configuration.image == nil {
      configuration.contentInsets = NSDirectionalEdgeInsets(
        top: 6, leading: 0, bottom: 6, trailing: 0)
    }
    button.configuration = configuration
    button.contentHorizontalAlignment = alignment == .leading ? .leading : .center
    button.isEnabled = enabled
    button.accessibilityHint = state.titleAction
  }
}

/// What sits under the navigation bar: on iPhone Duo the page title, then the
/// page's view tabs as a row of glass capsules that scrolls sideways.
///
/// A tab opens on tap. A long press lifts it: dragging moves it along the row,
/// releasing in place shows its menu (Close, for an added view).
final class PageAccessory: UIView, UIDragInteractionDelegate, UIDropInteractionDelegate,
  UIContextMenuInteractionDelegate
{
  struct Title: Equatable {
    let text: String
    let subtitle: String?
    @MainActor init(state: PageBarState) {
      text = state.title
      subtitle = state.subtitle
    }
  }

  static let titleHeight: CGFloat = 44
  static let tabsHeight: CGFloat = 48
  var onEvent: (([String: Any]) -> Void)?
  private let titleLabel = UILabel()
  private weak var titleButton: UIButton?
  private let scroll = UIScrollView()
  private let row = UIStackView()
  private var shown: (tabs: [PageBarTab], add: PageBarAddMenu?, enabled: Bool)?
  private var showsTitle = false
  private var showsTabs = false
  private var tabIds: [String] = []
  private var closeLabels: [String: String] = [:]
  private var glassStale = false
  let edgeInteraction: UIInteraction? = {
    if #available(iOS 26.0, *) {
      let interaction = UIScrollEdgeElementContainerInteraction()
      interaction.edge = .top
      return interaction
    }
    return nil
  }()

  override init(frame: CGRect) {
    super.init(frame: frame)
    titleLabel.numberOfLines = 1
    titleLabel.lineBreakMode = .byTruncatingTail
    scroll.showsHorizontalScrollIndicator = false
    scroll.clipsToBounds = false
    row.axis = .horizontal
    row.alignment = .center
    row.spacing = 8
    scroll.addSubview(row)
    addSubview(titleLabel)
    addSubview(scroll)
    scroll.addInteraction(UIDropInteraction(delegate: self))
    if let edgeInteraction { addInteraction(edgeInteraction) }
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  /// The height it takes; zero when it carries nothing.
  var height: CGFloat {
    (showsTitle ? Self.titleHeight : 0) + (showsTabs ? Self.tabsHeight : 0)
  }

  /// Glass capsules show flat once they come back from off screen or from
  /// a hidden layer, so they are rebuilt then.
  @MainActor
  func rebuildGlass() {
    guard let shown else { return }
    glassStale = true
    updateTabs(shown.tabs, add: shown.add, enabled: shown.enabled)
  }

  /// An edge swipe hides the accessory behind an image of it.
  @MainActor
  func setSwipeHidden(_ hidden: Bool) {
    layer.opacity = hidden ? 0 : 1
    if hidden {
      glassStale = true
    } else if glassStale {
      rebuildGlass()
    }
  }

  /// The page scroller whose top edge this extends. The strip joins the bar's
  /// edge effect only while it carries the Duo title, plain text that needs it;
  /// the view tabs alone are glass capsules with their own ground.
  func observe(_ scroller: UIScrollView?) {
    observed = scroller
    if #available(iOS 26.0, *) {
      (edgeInteraction as? UIScrollEdgeElementContainerInteraction)?.scrollView =
        showsTitle ? scroller : nil
    }
  }
  private weak var observed: UIScrollView?

  override func layoutSubviews() {
    super.layoutSubviews()
    let inset: CGFloat = 16
    let titleFrame = CGRect(
      x: inset, y: 0, width: bounds.width - inset * 2, height: showsTitle ? Self.titleHeight : 0)
    titleLabel.frame = titleFrame
    if let titleButton {
      // A glass title is a capsule at its own width.
      var frame = titleFrame
      if titleButton.configuration?.image != nil {
        frame.size.width = min(frame.width, titleButton.sizeThatFits(titleFrame.size).width)
      }
      titleButton.frame = frame
    }
    scroll.frame = CGRect(
      x: 0, y: titleFrame.maxY, width: bounds.width, height: showsTabs ? Self.tabsHeight : 0)
    let size = row.systemLayoutSizeFitting(UIView.layoutFittingCompressedSize)
    row.frame = CGRect(
      x: inset, y: (scroll.bounds.height - size.height) / 2, width: size.width, height: size.height)
    scroll.contentSize = CGSize(width: size.width + inset * 2, height: scroll.bounds.height)
  }

  @MainActor
  func update(
    title: Title?, titleButton: UIButton?, tabs: [PageBarTab]?, add: PageBarAddMenu?,
    enabled: Bool
  ) {
    showsTitle = title != nil || titleButton != nil
    showsTabs = tabs != nil
    observe(observed)
    if let title {
      let text = NSMutableAttributedString(
        string: title.text,
        attributes: [.font: UIFont.preferredFont(forTextStyle: .headline), .foregroundColor: UIColor.label])
      if let subtitle = title.subtitle {
        text.append(
          NSAttributedString(
            string: "  " + subtitle,
            attributes: [
              .font: UIFont.preferredFont(forTextStyle: .caption1),
              .foregroundColor: UIColor.secondaryLabel,
            ]))
      }
      titleLabel.attributedText = text
    }
    titleLabel.isHidden = title == nil
    if self.titleButton !== titleButton {
      self.titleButton?.removeFromSuperview()
      self.titleButton = titleButton
      if let titleButton { addSubview(titleButton) }
    }
    setNeedsLayout()
    updateTabs(tabs ?? [], add: add, enabled: enabled)
  }

  @MainActor
  private func updateTabs(_ tabs: [PageBarTab], add: PageBarAddMenu?, enabled: Bool) {
    // Rebuilding unchanged buttons would restart their glass animations.
    if !glassStale, let shown, shown.tabs == tabs, shown.add == add, shown.enabled == enabled {
      return
    }
    glassStale = layer.opacity == 0
    let selectionMoved =
      shown?.tabs.firstIndex { $0.item.selected } != tabs.firstIndex { $0.item.selected }
    shown = (tabs, add, enabled)
    tabIds = tabs.map(\.item.id)
    closeLabels = Dictionary(
      tabs.compactMap { tab in tab.closeLabel.map { (tab.item.id, $0) } }, uniquingKeysWith: { a, _ in a })
    for view in row.arrangedSubviews { view.removeFromSuperview() }
    for tab in tabs { row.addArrangedSubview(button(for: tab, enabled: enabled)) }
    if let add { row.addArrangedSubview(addButton(add, enabled: enabled)) }
    setNeedsLayout()
    layoutIfNeeded()
    if selectionMoved, let index = tabs.firstIndex(where: { $0.item.selected }) {
      let frame = row.arrangedSubviews[index].frame.offsetBy(dx: row.frame.minX, dy: 0)
      scroll.scrollRectToVisible(frame.insetBy(dx: -16, dy: 0), animated: false)
    }
  }

  @MainActor
  private func button(for tab: PageBarTab, enabled: Bool) -> UIButton {
    let selected = tab.item.selected
    var configuration = Self.capsule(prominent: selected)
    let ink: UIColor = selected ? .systemBackground : .label
    configuration.image = BarGlyphs.image(tab.item.icon, size: 16)?.withTintColor(
      ink, renderingMode: .alwaysOriginal)
    configuration.imagePadding = 6
    configuration.attributedTitle = AttributedString(
      tab.item.label,
      attributes: AttributeContainer([
        .font: UIFont.systemFont(ofSize: 15, weight: .semibold), .foregroundColor: ink,
      ]))
    configuration.titleLineBreakMode = .byTruncatingMiddle
    let id = tab.item.id
    let button = UIButton(
      configuration: configuration,
      primaryAction: UIAction { [weak self] _ in self?.onEvent?(["kind": "tab", "id": id]) })
    button.isEnabled = enabled
    button.accessibilityIdentifier = id
    button.widthAnchor.constraint(lessThanOrEqualToConstant: 220).isActive = true
    button.accessibilityLabel = tab.item.accessibilityLabel ?? tab.item.label
    if selected { button.accessibilityTraits.insert(.selected) }
    // A context-menu interaction, not `button.menu`: a button menu opens on the
    // long press itself and the drag never starts, while a context menu lifts
    // first and yields to the drag once the finger moves.
    if tab.closeLabel != nil { button.addInteraction(UIContextMenuInteraction(delegate: self)) }
    let drag = UIDragInteraction(delegate: self)
    drag.isEnabled = enabled
    button.addInteraction(drag)

    if tab.item.dot {
      let dot = UIView()
      dot.backgroundColor = .systemRed
      dot.layer.cornerRadius = 3
      dot.isUserInteractionEnabled = false
      dot.translatesAutoresizingMaskIntoConstraints = false
      button.addSubview(dot)
      NSLayoutConstraint.activate([
        dot.widthAnchor.constraint(equalToConstant: 6),
        dot.heightAnchor.constraint(equalToConstant: 6),
        dot.topAnchor.constraint(equalTo: button.topAnchor, constant: 6),
        dot.trailingAnchor.constraint(equalTo: button.trailingAnchor, constant: -8),
      ])
    }
    return button
  }

  @MainActor
  private func addButton(_ add: PageBarAddMenu, enabled: Bool) -> UIButton {
    var configuration = Self.capsule(prominent: false)
    configuration.image = UIImage(systemName: "plus")
    configuration.preferredSymbolConfigurationForImage = UIImage.SymbolConfiguration(
      pointSize: 14, weight: .semibold)
    let button = UIButton(configuration: configuration)
    button.accessibilityLabel = add.label
    button.isEnabled = enabled
    button.menu = UIMenu(
      children: add.items.map { view in
        UIAction(title: view.label, image: BarGlyphs.image(view.icon, size: 18)) { [weak self] _ in
          self?.onEvent?(["kind": "addTab", "id": view.id])
        }
      })
    button.showsMenuAsPrimaryAction = true
    return button
  }

  /// The selected tab takes the system selection in ink, like the bar's
  /// selected items. Glass dims its own foreground, so that capsule is a plain fill.
  private static func capsule(prominent: Bool) -> UIButton.Configuration {
    var configuration: UIButton.Configuration
    if !prominent, #available(iOS 26.0, *) {
      configuration = .glass()
    } else {
      configuration = prominent ? .filled() : .gray()
    }
    configuration.cornerStyle = .capsule
    configuration.baseForegroundColor = prominent ? .systemBackground : .label
    if prominent { configuration.baseBackgroundColor = .label }
    configuration.contentInsets = NSDirectionalEdgeInsets(
      top: 9, leading: 14, bottom: 9, trailing: 14)
    return configuration
  }

  // MARK: Menu

  func contextMenuInteraction(
    _ interaction: UIContextMenuInteraction, configurationForMenuAtLocation location: CGPoint
  ) -> UIContextMenuConfiguration? {
    guard let id = interaction.view?.accessibilityIdentifier, let closeLabel = closeLabels[id]
    else { return nil }
    return UIContextMenuConfiguration(actionProvider: { [weak self] _ in
      UIMenu(children: [
        UIAction(title: closeLabel, image: UIImage(systemName: "xmark"), attributes: .destructive) {
          _ in self?.onEvent?(["kind": "closeTab", "id": id])
        }
      ])
    })
  }

  func contextMenuInteraction(
    _ interaction: UIContextMenuInteraction,
    configuration: UIContextMenuConfiguration,
    highlightPreviewForItemWithIdentifier identifier: any NSCopying
  ) -> UITargetedPreview? {
    interaction.view.map(Self.capsulePreview)
  }

  func dragInteraction(
    _ interaction: UIDragInteraction, previewForLifting item: UIDragItem, session: UIDragSession
  ) -> UITargetedDragPreview? {
    guard let view = interaction.view else { return nil }
    let parameters = UIDragPreviewParameters()
    parameters.visiblePath = UIBezierPath(
      roundedRect: view.bounds, cornerRadius: view.bounds.height / 2)
    return UITargetedDragPreview(view: view, parameters: parameters)
  }

  private static func capsulePreview(_ view: UIView) -> UITargetedPreview {
    let parameters = UIPreviewParameters()
    parameters.visiblePath = UIBezierPath(
      roundedRect: view.bounds, cornerRadius: view.bounds.height / 2)
    return UITargetedPreview(view: view, parameters: parameters)
  }

  // MARK: Reordering

  func dragInteraction(
    _ interaction: UIDragInteraction, itemsForBeginning session: UIDragSession
  ) -> [UIDragItem] {
    guard let id = interaction.view?.accessibilityIdentifier else { return [] }
    let item = UIDragItem(itemProvider: NSItemProvider())
    item.localObject = id
    return [item]
  }

  func dragInteraction(
    _ interaction: UIDragInteraction, sessionIsRestrictedToDraggingApplication session: UIDragSession
  ) -> Bool { true }

  func dropInteraction(_ interaction: UIDropInteraction, canHandle session: UIDropSession) -> Bool {
    session.localDragSession?.items.first?.localObject is String
  }

  func dropInteraction(
    _ interaction: UIDropInteraction, sessionDidUpdate session: UIDropSession
  ) -> UIDropProposal {
    UIDropProposal(operation: .move)
  }

  func dropInteraction(_ interaction: UIDropInteraction, performDrop session: UIDropSession) {
    guard let id = session.localDragSession?.items.first?.localObject as? String,
      let from = tabIds.firstIndex(of: id)
    else { return }
    // The slot whose centre the drop passed.
    let x = session.location(in: row).x
    let centres = row.arrangedSubviews.prefix(tabIds.count).map(\.center.x)
    var to = centres.filter { $0 < x }.count
    if to > from { to -= 1 }
    to = min(max(to, 0), tabIds.count - 1)
    if to != from { onEvent?(["kind": "moveTab", "id": id, "to": to]) }
  }
}

struct TitlebarState: Equatable {
  struct Sidebar: Equatable {
    let shown: Bool
    let label: String
  }
  /// The browse sidebar toggle; nil hides it.
  let sidebar: Sidebar?
  /// A status label beside the toggle; nil shows none.
  let status: String?
  let searchPlaceholder: String
  let searchShortcut: String
  /// The window frame behind the panes is dark.
  let dark: Bool
  /// The page header's buttons, drawn beside the vertical bar's own.
  let actions: [BarAction]
}

/// The wide layout's window titlebar: the sidebar toggle, the archive status
/// and the global search field. The web page draws the search results below
/// it and receives the field's text and keys.
///
/// On an open iPhone Duo the vertical bar stands in for the titlebar row:
/// search and the sidebar toggle sit above its tabs, and search opens the
/// page's own search field.
@MainActor
final class Titlebar: NSObject, UITextFieldDelegate {
  var state: TitlebarState?
  var onEvent: (([String: Any]) -> Void)?
  private let field = TitlebarSearchField()
  private lazy var fieldWidth = field.widthAnchor.constraint(equalToConstant: 420)
  private lazy var sidebar = UIBarButtonItem(
    image: nil,
    primaryAction: UIAction { [weak self] _ in self?.onEvent?(["kind": "toggleSidebar"]) })
  private lazy var search = UIBarButtonItem(
    image: UIImage(systemName: "magnifyingglass"),
    primaryAction: UIAction { [weak self] _ in self?.focusSearch() })
  private var vertical = false
  private let status: UIBarButtonItem = {
    let item = UIBarButtonItem(title: nil)
    item.isEnabled = false
    return item
  }()

  override init() {
    super.init()
    field.delegate = self
    field.translatesAutoresizingMaskIntoConstraints = false
    fieldWidth.isActive = true
    // As tall as the glass bar buttons beside it.
    field.heightAnchor.constraint(equalToConstant: 44).isActive = true
    field.addTarget(self, action: #selector(began), for: .editingDidBegin)
    field.addTarget(self, action: #selector(changed), for: .editingChanged)
    field.onKey = { [weak self] key in self?.onEvent?(["kind": "searchKey", "key": key]) }
    sidebar.tintColor = .label
    search.tintColor = .label
  }

  /// `width` is the window's; the field follows the desktop pill's
  /// `clamp(420px, 52vw, 720px)`.
  func apply(to item: UINavigationItem, width: CGFloat, vertical: Bool, enabled: Bool) {
    guard let state else { return }
    fieldWidth.constant = min(720, max(420, width * 0.52))
    field.isEnabled = enabled
    sidebar.isEnabled = enabled
    search.isEnabled = enabled
    field.placeholder = state.searchPlaceholder
    search.accessibilityLabel = state.searchShortcut
    // The desktop glyphs: the chevron points at what a tap does.
    if let toggle = state.sidebar {
      sidebar.image = UIImage(named: toggle.shown ? "TitlebarSidebarClose" : "TitlebarSidebarOpen")
      sidebar.accessibilityLabel = toggle.label
    }
    status.title = state.status
    self.vertical = vertical

    if vertical, #available(iOS 27.1, *) {
      let page = state.actions.map { action in
        let item = BarGlyphs.item(for: action) { [weak self] id in
          self?.onEvent?(["kind": "action", "id": id])
        }
        item.isEnabled = enabled
        return [item]
      }
      let groups = [[search]] + (state.sidebar == nil ? [] : [[sidebar]]) + page
      for item in groups.joined() { item.axisBehavior = .verticalPreferred }
      item.verticalBarCompressionBehavior = .prefersTabBar
      item.trailingItemGroups = BarGlyphs.groups(groups)
      return
    }

    if item.titleView !== field { item.titleView = field }
    item.leftBarButtonItems =
      (state.sidebar == nil ? [] : [sidebar]) + (state.status == nil ? [] : [status])
  }

  /// Beside the vertical bar the page draws the search field itself.
  func focusSearch() {
    if vertical {
      onEvent?(["kind": "searchOpen"])
    } else {
      field.becomeFirstResponder()
    }
  }

  func endSearch(clear: Bool) {
    if clear { field.text = "" }
    field.resignFirstResponder()
  }

  @objc private func began() { onEvent?(["kind": "searchFocus"]) }

  /// Composing text (marked IME input) is not a query yet.
  @objc private func changed() {
    guard field.markedTextRange == nil else { return }
    onEvent?(["kind": "searchQuery", "query": field.text ?? ""])
  }

  func textFieldShouldReturn(_ textField: UITextField) -> Bool {
    onEvent?(["kind": "searchKey", "key": "enter"])
    return false
  }
}

/// Hands the result-list keys to the page while the field keeps the text.
final class TitlebarSearchField: UISearchTextField {
  var onKey: ((String) -> Void)?

  override var keyCommands: [UIKeyCommand]? {
    let keys: [(String, UIKeyModifierFlags, String)] = [
      (UIKeyCommand.inputUpArrow, [], "up"),
      (UIKeyCommand.inputDownArrow, [], "down"),
      ("\t", [], "tab"),
      ("\t", .shift, "backtab"),
      (UIKeyCommand.inputEscape, [], "escape"),
    ]
    return keys.map { input, modifiers, key in
      let command = UIKeyCommand(
        title: "", action: #selector(pressed(_:)), input: input, modifierFlags: modifiers,
        propertyList: key)
      command.wantsPriorityOverSystemBehavior = true
      return command
    }
  }

  @objc private func pressed(_ command: UIKeyCommand) {
    if let key = command.propertyList as? String { onKey?(key) }
  }
}
