import PhotosUI
import UIKit
import UniformTypeIdentifiers

/// An item of the add menu: a system picker native runs, or a tap the page handles.
struct ComposerMenuItem: Equatable {
  let id, label, icon: String
  let pick: String?
}

struct ComposerState: Equatable {
  let key, placeholder: String
  let enabled, canSend: Bool
  let sendLabel, addLabel: String
  /// The page's controls floating above the composer.
  let accessoryHeight: CGFloat
  let add: [ComposerMenuItem]
  let picker: ComposerPicker?
  let activity: ComposerActivity?
  /// The pane it belongs to, in the host's points; nil spans the screen.
  let frame: ComposerFrame?
}

struct ComposerFrame: Equatable {
  /// `bottom` is the pane's end, up from the window's bottom.
  let left, width, bottom: CGFloat
}

/// The native composer, floating over the page above the keyboard. The page
/// owns the draft, the send and every string; this edits text and reports it.
///
/// Without a `frame` it spans the screen; with one it stands over its pane,
/// beside the other panes' composers, and rests at the pane's end while
/// another one edits.
///
/// At rest it is a one-line capsule; editing opens it into a card whose text
/// grows with its lines up to `autoLines`, then scrolls. The grabber drags
/// the card taller; dragged back near one line it follows its text again, and
/// flung down it closes. `reserve` (the top at the text's own height) is what
/// the page clears: a dragged height covers the page rather than moving it.
@MainActor
final class ComposerView: UIView, UITextViewDelegate, UIGestureRecognizerDelegate,
  PHPickerViewControllerDelegate, UIDocumentPickerDelegate
{
  /// Edits and taps, and `metrics`: distances up from the bottom of the host
  /// view to the composer's top at rest (`rest`), at the height its text gives
  /// it (`reserve`), and as drawn (`overlay`); and the room the picker or the
  /// activity line takes over it (`picker`).
  var onEvent: (([String: Any]) -> Void)?
  /// Presents the system pickers.
  weak var presenter: UIViewController?
  private(set) var state: ComposerState?
  /// Where the page's bars end; a card dragged to its tallest stays below them.
  var topLimit: CGFloat = 0

  private static let autoLines: CGFloat = 6
  private static let button: CGFloat = 36
  private static let padding: CGFloat = 8
  /// From the screen's or the pane's sides, and from a pane's end.
  private static let inset: CGFloat = 12
  private static let cardTop: CGFloat = 16
  private static let cardRadius: CGFloat = 26
  private static let grabberZone: CGFloat = 28
  private static let snapDistance: CGFloat = 24
  /// How far into the home indicator's inset the composer sits with the
  /// keyboard down, level with the system's floating bars (the vertical tab
  /// bar on iPhone Duo ends 24pt above the bottom, inside a 34pt inset).
  private static let homeOverlap: CGFloat = 10

  private let backdrop: UIVisualEffectView
  private let picker = ComposerPickerView()
  private let activity = ComposerActivityView()
  private var pickerHeight: NSLayoutConstraint?
  private let textView = ComposerTextView()
  private let placeholder = UILabel()
  private let grabber = UIView()
  private let addButton = UIButton(configuration: .plain())
  private let sendButton = UIButton(configuration: .filled())
  private var heightConstraint: NSLayoutConstraint?
  // Sides: the page's safe area, or the pane's frame (`placeSides`).
  private var screenSides: [NSLayoutConstraint] = []
  private var paneSides: (leading: NSLayoutConstraint, width: NSLayoutConstraint)?
  // Bottom (`placeBottom`): never below rest (`restCap`); above the keyboard
  // while it follows it, else at rest. Beside other panes' composers only
  // the one editing follows the keyboard.
  private var restCap: NSLayoutConstraint?
  private var restBottom: NSLayoutConstraint?
  private var keyboardBottom: [NSLayoutConstraint] = []
  private(set) var isEditing = false
  /// The text's height the user dragged to; nil follows the text.
  private var draggedHeight: CGFloat?
  private var dragStart: CGFloat = 0
  private var published: [CGFloat]?

  override init(frame: CGRect) {
    if #available(iOS 26.0, *) {
      backdrop = UIVisualEffectView(effect: UIGlassEffect(style: .regular))
    } else {
      backdrop = UIVisualEffectView(effect: UIBlurEffect(style: .systemChromeMaterial))
    }
    super.init(frame: frame)
    addSubview(backdrop)

    let font = UIFont.preferredFont(forTextStyle: .body)
    textView.font = font
    textView.adjustsFontForContentSizeCategory = true
    textView.backgroundColor = .clear
    textView.textColor = .label
    textView.textContainerInset = UIEdgeInsets(top: 8, left: 0, bottom: 8, right: 0)
    textView.textContainer.lineFragmentPadding = 0
    textView.delegate = self
    textView.onPasteFiles = { [weak self] files in self?.sendFiles(files) }
    textView.onReturn = { [weak self] in self?.hardwareReturn() }
    backdrop.contentView.addSubview(textView)

    placeholder.font = font
    placeholder.adjustsFontForContentSizeCategory = true
    placeholder.textColor = .placeholderText
    placeholder.isUserInteractionEnabled = false
    backdrop.contentView.addSubview(placeholder)

    grabber.backgroundColor = .tertiaryLabel
    grabber.layer.cornerRadius = 2.5
    grabber.alpha = 0
    backdrop.contentView.addSubview(grabber)

    // The same round ground the send button rests on while empty.
    var add = UIButton.Configuration.filled()
    add.cornerStyle = .capsule
    add.image = UIImage(
      systemName: "plus",
      withConfiguration: UIImage.SymbolConfiguration(pointSize: 13, weight: .medium))
    add.baseBackgroundColor = .tertiarySystemFill
    add.baseForegroundColor = .label
    addButton.configuration = add
    addButton.showsMenuAsPrimaryAction = true
    backdrop.contentView.addSubview(addButton)

    sendButton.addAction(
      UIAction { [weak self] _ in self?.send() }, for: .primaryActionTriggered)
    backdrop.contentView.addSubview(sendButton)

    let pan = UIPanGestureRecognizer(target: self, action: #selector(drag(_:)))
    pan.delegate = self
    addGestureRecognizer(pan)
    activity.alpha = 0
    activity.addAction(
      UIAction { [weak self] _ in
        guard let key = self?.state?.key, let id = self?.activity.activity?.id else { return }
        self?.onEvent?(["kind": "activity", "key": key, "id": id])
      }, for: .touchUpInside)
    picker.alpha = 0
    picker.onPick = { [weak self] id in
      guard let key = self?.state?.key else { return }
      self?.onEvent?(["kind": "pick", "key": key, "id": id])
    }

    // A tap on the capsule anywhere starts editing, as on its text.
    let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
    tap.cancelsTouchesInView = false
    addGestureRecognizer(tap)
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  /// Places the composer in `host`, its bottom above the keyboard and its
  /// sides, screen-wide, inside `area`.
  func install(in host: UIView, area: UILayoutGuide) {
    guard superview !== host else { return }
    host.addSubview(self)
    translatesAutoresizingMaskIntoConstraints = false
    let guide = host.keyboardLayoutGuide
    // Hidden, the guide sits at the bottom edge rather than the safe area, so
    // the composer's own cap below decides its resting place.
    if #available(iOS 17.0, *) { guide.usesBottomSafeArea = false }
    // Leaving a superview drops only the constraints to it; the height is
    // the composer's own and would otherwise pile up, one per install.
    heightConstraint?.isActive = false
    let height = heightAnchor.constraint(equalToConstant: collapsedHeight)
    let pull = bottomAnchor.constraint(equalTo: guide.topAnchor, constant: -Self.padding)
    pull.priority = .defaultHigh
    heightConstraint = height
    screenSides = [
      leftAnchor.constraint(equalTo: area.leftAnchor, constant: Self.inset),
      rightAnchor.constraint(equalTo: area.rightAnchor, constant: -Self.inset),
    ]
    paneSides = (
      leadingAnchor.constraint(equalTo: host.leadingAnchor),
      widthAnchor.constraint(equalToConstant: 0)
    )
    keyboardBottom = [
      bottomAnchor.constraint(lessThanOrEqualTo: guide.topAnchor, constant: -Self.padding), pull,
    ]
    let rest = bottomAnchor.constraint(equalTo: host.bottomAnchor)
    rest.priority = .defaultHigh
    restBottom = rest
    restCap?.isActive = false
    let cap = bottomAnchor.constraint(lessThanOrEqualTo: host.bottomAnchor)
    restCap = cap
    NSLayoutConstraint.activate([cap, height])
    placeSides()
    placeBottom()

    host.addSubview(picker)
    picker.translatesAutoresizingMaskIntoConstraints = false
    pickerHeight?.isActive = false
    let listHeight = picker.heightAnchor.constraint(equalToConstant: 0)
    pickerHeight = listHeight
    NSLayoutConstraint.activate([
      picker.leadingAnchor.constraint(equalTo: leadingAnchor),
      picker.trailingAnchor.constraint(equalTo: trailingAnchor),
      picker.bottomAnchor.constraint(equalTo: topAnchor, constant: -Self.padding),
      listHeight,
    ])

    host.addSubview(activity)
    activity.translatesAutoresizingMaskIntoConstraints = false
    NSLayoutConstraint.activate([
      activity.leadingAnchor.constraint(equalTo: leadingAnchor),
      activity.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
      activity.bottomAnchor.constraint(equalTo: topAnchor, constant: -Self.padding),
    ])
  }

  private func placeSides() {
    guard let paneSides else { return }
    if let frame = state?.frame {
      NSLayoutConstraint.deactivate(screenSides)
      paneSides.leading.constant = frame.left + Self.inset
      paneSides.width.constant = max(0, frame.width - Self.inset * 2)
      NSLayoutConstraint.activate([paneSides.leading, paneSides.width])
    } else {
      NSLayoutConstraint.deactivate([paneSides.leading, paneSides.width])
      NSLayoutConstraint.activate(screenSides)
    }
  }

  private func placeBottom() {
    restBottom?.constant = -restInset
    restCap?.constant = -restInset
    let follows = state?.frame == nil || isEditing
    restBottom?.isActive = !follows
    if follows { NSLayoutConstraint.activate(keyboardBottom) } else {
      NSLayoutConstraint.deactivate(keyboardBottom)
    }
  }

  func apply(_ next: ComposerState) {
    let previous = state
    state = next
    if previous?.frame != next.frame {
      placeSides()
      placeBottom()
    }
    if previous?.key != next.key {
      textView.text = ""
      draggedHeight = nil
      if textView.isFirstResponder { textView.resignFirstResponder() }
    }
    textView.isEditable = next.enabled
    placeholder.text = next.placeholder
    addButton.isEnabled = next.enabled
    addButton.accessibilityLabel = next.addLabel
    addButton.menu = UIMenu(children: next.add.map { item in
      UIAction(title: item.label, image: BarGlyphs.image(item.icon, size: 18)) { [weak self] _ in
        self?.choose(item)
      }
    })
    sendButton.accessibilityLabel = next.sendLabel
    styleSend(enabled: next.enabled && next.canSend)
    showPicker(next.picker)
    showActivity(next.picker == nil ? next.activity : nil)
    refresh(animated: false)
  }

  private func showPicker(_ next: ComposerPicker?) {
    let opening = picker.picker == nil && next != nil
    if let next { picker.apply(next) }
    superview?.layoutIfNeeded()
    let height = next == nil ? 0 : picker.preferredHeight
    if pickerHeight?.constant != height || opening {
      pickerHeight?.constant = height
      if opening { superview?.layoutIfNeeded() }
    }
    let shown = next != nil
    UIView.animate(withDuration: 0.18, delay: 0, options: [.beginFromCurrentState]) {
      self.picker.alpha = shown ? 1 : 0
    } completion: { _ in
      if !shown, self.state?.picker == nil { self.picker.apply(nil) }
    }
    publishMetrics()
  }

  /// The activity line gives way to the picker, which takes the same room.
  private func showActivity(_ next: ComposerActivity?) {
    if let next { activity.apply(next) }
    let shown = next != nil
    UIView.animate(withDuration: 0.18, delay: 0, options: [.beginFromCurrentState]) {
      self.activity.alpha = shown ? 1 : 0
    } completion: { _ in
      if !shown, self.activityShown == false { self.activity.apply(nil) }
    }
    activityShown = shown
    publishMetrics()
  }

  private var activityShown = false

  /// Replaces the text the page holds for draft `key`.
  func setText(key: String, text: String, caret: Int, focus: Bool) {
    guard state?.key == key else { return }
    if textView.text != text {
      textView.text = text
      if text.isEmpty { draggedHeight = nil }
    }
    let length = (text as NSString).length
    textView.selectedRange = NSRange(location: min(max(caret, 0), length), length: 0)
    if focus, state?.enabled == true, !textView.isFirstResponder { textView.becomeFirstResponder() }
    refresh(animated: false)
  }

  /// Whether a tap on the page at `point` ends editing: any tap but one on
  /// the controls the page floats above the composer.
  func endsEditing(at point: CGPoint) -> Bool {
    guard textView.isFirstResponder, let state else { return false }
    return point.y < frame.minY - listSpace - state.accessoryHeight - Self.padding * 2
      || point.x < frame.minX || point.x > frame.maxX
  }

  func endEditing() { textView.resignFirstResponder() }

  /// Keeps the keyboard while a touch on the page's controls above the
  /// composer lasts: the page would otherwise take focus from it.
  func holdFocus(_ held: Bool, at point: CGPoint) {
    if held {
      textView.holdsFocus = textView.isFirstResponder && !endsEditing(at: point)
    } else {
      // The page takes focus as the touch ends, after this.
      DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
        self?.textView.holdsFocus = false
      }
    }
  }

  /// An edge swipe carries the page, and the composer with it, `progress`
  /// of the way off; nil puts it back.
  func setBackProgress(_ progress: CGFloat?) {
    let share = min(1, max(0, progress ?? 0))
    transform = CGAffineTransform(translationX: (superview?.bounds.width ?? 0) * share, y: 0)
    picker.transform = transform
    activity.transform = transform
  }

  /// A web modal covers the page; native chrome cannot sit under its
  /// backdrop, so the composer leaves while one is open.
  func setBlocked(_ blocked: Bool) {
    self.blocked = blocked
    applyHidden()
  }

  /// The message-selection bar takes the composer's place while it shows.
  func setSteppedAside(_ aside: Bool) {
    steppedAside = aside
    applyHidden()
  }

  private var blocked = false
  private var steppedAside = false

  private func applyHidden() {
    let hidden = blocked || steppedAside
    if hidden, textView.isFirstResponder { textView.resignFirstResponder() }
    isHidden = hidden
    picker.isHidden = hidden
    activity.isHidden = hidden
  }

  /// Takes the composer off screen and forgets its draft.
  func remove() {
    if textView.isFirstResponder { textView.resignFirstResponder() }
    if let key = state?.key {
      onEvent?(["kind": "metrics", "key": key, "rest": 0, "reserve": 0, "overlay": 0, "picker": 0])
    }
    state = nil
    textView.text = ""
    draggedHeight = nil
    isEditing = false
    transform = .identity
    picker.transform = .identity
    picker.alpha = 0
    picker.apply(nil)
    pickerHeight?.constant = 0
    picker.removeFromSuperview()
    activity.transform = .identity
    activity.alpha = 0
    activity.apply(nil)
    activityShown = false
    activity.removeFromSuperview()
    removeFromSuperview()
    published = nil
  }

  // MARK: Layout

  private var font: UIFont { textView.font ?? .preferredFont(forTextStyle: .body) }
  private var lineBox: CGFloat { ceil(font.lineHeight) + 16 }
  private var collapsedHeight: CGFloat { max(Self.button, lineBox) + Self.padding * 2 }
  private var cardChrome: CGFloat { Self.cardTop + Self.padding * 2 + Self.button }

  /// The text's height in the card when it follows its lines.
  private func autoTextHeight(width: CGFloat) -> CGFloat {
    let content = textView.sizeThatFits(CGSize(width: width, height: .greatestFiniteMagnitude))
      .height
    let cap = ceil(font.lineHeight * Self.autoLines) + 16
    return min(max(content, lineBox), cap)
  }

  /// The tallest text a dragged card can hold, its top kept below the page's bar.
  private var maxTextHeight: CGFloat {
    guard let host = superview else { return lineBox }
    let bottom = frame.maxY > 0 ? frame.maxY : host.bounds.maxY - Self.padding
    return max(lineBox, bottom - max(topLimit, host.safeAreaInsets.top) - Self.padding - cardChrome)
  }

  private var textWidth: CGFloat { max(0, bounds.width - 32) }

  private func heights() -> (drawn: CGFloat, reserve: CGFloat) {
    guard isEditing else { return (collapsedHeight, collapsedHeight) }
    let auto = autoTextHeight(width: textWidth) + cardChrome
    let dragged = draggedHeight.map { min($0, maxTextHeight) + cardChrome }
    return (dragged ?? auto, auto)
  }

  private func refresh(animated: Bool) {
    placeholder.isHidden = !textView.text.isEmpty
    let (drawn, _) = heights()
    guard heightConstraint?.constant != drawn || animated else {
      setNeedsLayout()
      return
    }
    heightConstraint?.constant = drawn
    guard animated, window != nil, !UIAccessibility.isReduceMotionEnabled else {
      superview?.setNeedsLayout()
      return
    }
    UIView.animate(
      withDuration: 0.38, delay: 0, usingSpringWithDamping: 0.86, initialSpringVelocity: 0,
      options: [.allowUserInteraction, .beginFromCurrentState]
    ) {
      self.superview?.layoutIfNeeded()
    }
  }

  override func layoutSubviews() {
    super.layoutSubviews()
    backdrop.frame = bounds
    if #available(iOS 26.0, *) {
      backdrop.cornerConfiguration =
        isEditing ? .uniformCorners(radius: .fixed(Self.cardRadius)) : .capsule()
    } else {
      backdrop.layer.cornerRadius = isEditing ? Self.cardRadius : bounds.height / 2
      backdrop.clipsToBounds = true
    }
    let pad = Self.padding
    let side = Self.button
    let buttonsY = bounds.height - pad - side
    addButton.frame = CGRect(x: pad, y: buttonsY, width: side, height: side)
    sendButton.frame = CGRect(x: bounds.width - pad - side, y: buttonsY, width: side, height: side)
    if isEditing {
      textView.frame = CGRect(
        x: 16, y: Self.cardTop, width: textWidth, height: max(0, buttonsY - pad - Self.cardTop))
      grabber.frame = CGRect(x: (bounds.width - 36) / 2, y: 6, width: 36, height: 5)
    } else {
      let x = pad + side + pad
      textView.frame = CGRect(
        x: x, y: (bounds.height - lineBox) / 2, width: max(0, bounds.width - x * 2),
        height: lineBox)
    }
    textView.isScrollEnabled = true
    grabber.alpha = isEditing ? 1 : 0
    placeholder.frame = CGRect(
      x: textView.frame.minX, y: textView.frame.minY + 8, width: textView.frame.width,
      height: ceil(font.lineHeight))
    publishMetrics()
  }

  /// The composer's distance up from the window's bottom with the keyboard
  /// down; in a pane that ends above the window (iPad's frame), as far in
  /// from the pane's end as from its sides.
  private var restInset: CGFloat {
    let home = max(Self.padding, (superview?.safeAreaInsets.bottom ?? 0) - Self.homeOverlap)
    guard let frame = state?.frame, frame.bottom > 0 else { return home }
    return max(home, frame.bottom + Self.inset)
  }

  /// Tells the page where the composer's top is; the host calls this after
  /// each layout pass, as the keyboard moves the composer without resizing it.
  func publishMetrics() {
    guard let host = superview, let key = state?.key else { return }
    let bottom = host.bounds.maxY - frame.maxY
    let rest = restInset + collapsedHeight
    // The activity line stays while agents run, so the page clears it as it
    // clears the composer; the picker is passing and covers the page instead.
    let lasting = activityShown && state?.picker == nil ? listSpace : 0
    let next = [rest, bottom + heights().reserve + lasting, bottom + frame.height, listSpace]
    guard published != next else { return }
    published = next
    onEvent?([
      "kind": "metrics", "key": key, "rest": next[0], "reserve": next[1], "overlay": next[2],
      "picker": next[3],
    ])
  }

  /// The room the picker or the activity line takes over the composer.
  private var listSpace: CGFloat {
    if state?.picker != nil { return (pickerHeight?.constant ?? 0) + Self.padding }
    return activityShown ? ComposerActivityView.height + Self.padding : 0
  }

  // MARK: Editing

  @objc private func tapped(_ tap: UITapGestureRecognizer) {
    guard state?.enabled == true, !textView.isFirstResponder else { return }
    for button in [addButton, sendButton] where button.frame.contains(tap.location(in: self)) {
      return
    }
    textView.becomeFirstResponder()
  }

  func textViewDidBeginEditing(_ textView: UITextView) {
    isEditing = true
    placeBottom()
    refresh(animated: true)
  }

  func textViewDidEndEditing(_ textView: UITextView) {
    isEditing = false
    placeBottom()
    refresh(animated: true)
    textView.setContentOffset(.zero, animated: false)
  }

  func textViewDidChange(_ textView: UITextView) {
    report()
    refresh(animated: false)
  }

  func textViewDidChangeSelection(_ textView: UITextView) {
    if textView.isFirstResponder { report() }
  }

  private func report() {
    guard let state else { return }
    onEvent?([
      "kind": "change", "key": state.key, "text": textView.text ?? "",
      "caret": textView.selectedRange.location + textView.selectedRange.length,
      "composing": textView.markedTextRange != nil,
    ])
  }

  /// A hardware keyboard's Return sends, as on the desktop, or picks the
  /// first candidate while the picker lists some; Shift-Return adds a line.
  private func hardwareReturn() {
    if textView.markedTextRange != nil {
      textView.unmarkText()
      return
    }
    if let key = state?.key, let first = state?.picker?.rows.first {
      onEvent?(["kind": "pick", "key": key, "id": first.id])
      return
    }
    send()
  }

  private func send() {
    guard let state, state.enabled, state.canSend else { return }
    // Marked text is not yet part of the draft the page holds.
    textView.unmarkText()
    onEvent?([
      "kind": "send", "key": state.key, "text": textView.text ?? "",
      "caret": textView.selectedRange.location + textView.selectedRange.length,
    ])
  }

  private func styleSend(enabled: Bool) {
    var configuration = UIButton.Configuration.filled()
    configuration.cornerStyle = .capsule
    // Colours resolved here and drawn as is: the glass treats a template
    // glyph as vibrant and greys it, so a live arrow read as disabled.
    let background = (enabled ? UIColor.label : .tertiarySystemFill).resolvedColor(
      with: traitCollection)
    let foreground = (enabled ? UIColor.systemBackground : .tertiaryLabel).resolvedColor(
      with: traitCollection)
    configuration.image = UIImage(
      systemName: "arrow.up", withConfiguration: UIImage.SymbolConfiguration(weight: .semibold)
    )?.withTintColor(foreground, renderingMode: .alwaysOriginal)
    configuration.baseBackgroundColor = background
    sendButton.configuration = configuration
    sendButton.isEnabled = enabled
    sendEnabled = enabled
  }

  private var sendEnabled = false

  override func traitCollectionDidChange(_ previous: UITraitCollection?) {
    super.traitCollectionDidChange(previous)
    if previous?.userInterfaceStyle != traitCollection.userInterfaceStyle {
      styleSend(enabled: sendEnabled)
    }
  }


  // MARK: Grabber

  /// Where the touch began: a pan begins only after it has moved.
  private var touchStart: CGFloat = .infinity

  func gestureRecognizer(_ recognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
    if recognizer is UIPanGestureRecognizer { touchStart = touch.location(in: self).y }
    return true
  }

  override func gestureRecognizerShouldBegin(_ recognizer: UIGestureRecognizer) -> Bool {
    guard recognizer is UIPanGestureRecognizer else {
      return super.gestureRecognizerShouldBegin(recognizer)
    }
    return isEditing && touchStart < Self.grabberZone
  }

  @objc private func drag(_ pan: UIPanGestureRecognizer) {
    let translation = pan.translation(in: self).y
    switch pan.state {
    case .began:
      dragStart = draggedHeight ?? autoTextHeight(width: textWidth)
    case .changed:
      draggedHeight = min(max(dragStart - translation, lineBox), maxTextHeight)
      heightConstraint?.constant = heights().drawn
      superview?.layoutIfNeeded()
    case .ended, .cancelled:
      let velocity = pan.velocity(in: self).y
      if velocity > 900 && translation > 40 {
        textView.resignFirstResponder()
        return
      }
      if let dragged = draggedHeight, dragged < lineBox + Self.snapDistance {
        draggedHeight = nil
      }
      refresh(animated: true)
    default:
      break
    }
  }

  // MARK: Add

  private func choose(_ item: ComposerMenuItem) {
    guard let state else { return }
    switch item.pick {
    case "photos": pickPhotos()
    case "files": pickFiles()
    default: onEvent?(["kind": "add", "key": state.key, "id": item.id])
    }
  }

  private func present(_ controller: UIViewController) {
    guard let presenter, presenter.presentedViewController == nil else { return }
    presenter.present(controller, animated: true)
  }

  private func pickPhotos() {
    var configuration = PHPickerConfiguration()
    configuration.filter = .images
    configuration.selectionLimit = 0
    configuration.preferredAssetRepresentationMode = .compatible
    let picker = PHPickerViewController(configuration: configuration)
    picker.delegate = self
    present(picker)
  }

  private func pickFiles() {
    let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: true)
    picker.allowsMultipleSelection = true
    picker.delegate = self
    present(picker)
  }

  nonisolated func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
    Task { @MainActor in
      picker.dismiss(animated: true)
      guard let key = state?.key else { return }
      var files: [ComposerFile] = []
      for result in results {
        if let file = await Self.load(result.itemProvider) { files.append(file) }
      }
      sendFiles(files, key: key)
    }
  }

  private nonisolated static func load(_ provider: NSItemProvider) async -> ComposerFile? {
    await withCheckedContinuation { continuation in
      provider.loadFileRepresentation(forTypeIdentifier: UTType.image.identifier) { url, _ in
        guard let url, let data = try? Data(contentsOf: url) else {
          continuation.resume(returning: nil)
          return
        }
        let type = UTType(filenameExtension: url.pathExtension)
        let base = provider.suggestedName ?? "image"
        let name = url.pathExtension.isEmpty ? base : base + "." + url.pathExtension
        continuation.resume(
          returning: ComposerFile(name: name, type: type?.preferredMIMEType ?? "", data: data))
      }
    }
  }

  nonisolated func documentPicker(
    _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
  ) {
    let files = urls.compactMap { url -> ComposerFile? in
      guard let data = try? Data(contentsOf: url) else { return nil }
      let type = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? ""
      return ComposerFile(name: url.lastPathComponent, type: type, data: data)
    }
    Task { @MainActor in self.sendFiles(files) }
  }

  private func sendFiles(_ files: [ComposerFile], key: String? = nil) {
    guard let state, key == nil || key == state.key, !files.isEmpty else { return }
    onEvent?([
      "kind": "files", "key": state.key,
      "files": files.map {
        ["name": $0.name, "type": $0.type, "base64": $0.data.base64EncodedString()]
      },
    ])
  }
}

struct ComposerFile: Sendable {
  let name, type: String
  let data: Data
}

/// Pasted images become attachments; text pastes as usual.
final class ComposerTextView: UITextView {
  var onPasteFiles: (([ComposerFile]) -> Void)?
  var onReturn: (() -> Void)?

  override var keyCommands: [UIKeyCommand]? {
    let command = UIKeyCommand(input: "\r", modifierFlags: [], action: #selector(pressedReturn))
    command.wantsPriorityOverSystemBehavior = true
    return [command]
  }

  @objc private func pressedReturn() { onReturn?() }
  /// The page takes focus on any tap; one on the controls above the composer
  /// must leave the keyboard where it is.
  var holdsFocus = false

  override func resignFirstResponder() -> Bool {
    return holdsFocus ? false : super.resignFirstResponder()
  }

  override func canPerformAction(_ action: Selector, withSender sender: Any?) -> Bool {
    if action == #selector(paste(_:)), UIPasteboard.general.hasImages { return true }
    return super.canPerformAction(action, withSender: sender)
  }

  override func paste(_ sender: Any?) {
    let board = UIPasteboard.general
    guard !board.hasStrings, board.hasImages else {
      super.paste(sender)
      return
    }
    let files = (board.images ?? []).enumerated().compactMap { index, image -> ComposerFile? in
      guard let data = image.pngData() else { return nil }
      return ComposerFile(name: "pasted-image-\(index + 1).png", type: "image/png", data: data)
    }
    onPasteFiles?(files)
  }
}
