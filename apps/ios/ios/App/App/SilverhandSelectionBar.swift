import UIKit

struct SelectionBarState: Equatable {
  let key, count, cancelLabel, copyLabel, saveLabel: String
  let enabled: Bool
  /// The image is rendering: Save shows it is at work.
  let busy: Bool
}

/// The message-selection actions in the composer's place and glass: Cancel,
/// the count, Copy and Save. The page keeps the selection and every string;
/// this reports a tap.
@MainActor
final class SelectionBarView: UIView {
  var onEvent: (([String: Any]) -> Void)?
  private(set) var state: SelectionBarState?

  private static let button: CGFloat = 36
  private static let padding: CGFloat = 8
  /// As the composer: level with the system's floating bars.
  private static let homeOverlap: CGFloat = 10

  private let backdrop: UIVisualEffectView
  private let cancelButton = UIButton(configuration: .plain())
  private let countLabel = UILabel()
  private let copyButton = UIButton(configuration: .filled())
  private let saveButton = UIButton(configuration: .filled())

  override init(frame: CGRect) {
    if #available(iOS 26.0, *) {
      backdrop = UIVisualEffectView(effect: UIGlassEffect(style: .regular))
      backdrop.cornerConfiguration = .capsule()
    } else {
      backdrop = UIVisualEffectView(effect: UIBlurEffect(style: .systemChromeMaterial))
      backdrop.clipsToBounds = true
    }
    super.init(frame: frame)
    addSubview(backdrop)

    countLabel.font = .preferredFont(forTextStyle: .subheadline)
    countLabel.adjustsFontForContentSizeCategory = true
    countLabel.textColor = .secondaryLabel
    countLabel.textAlignment = .center
    countLabel.lineBreakMode = .byTruncatingTail

    for (button, kind) in [(cancelButton, "cancel"), (copyButton, "copy"), (saveButton, "save")] {
      button.addAction(
        UIAction { [weak self] _ in
          guard let key = self?.state?.key else { return }
          self?.onEvent?(["kind": kind, "key": key])
        }, for: .primaryActionTriggered)
      backdrop.contentView.addSubview(button)
    }
    backdrop.contentView.addSubview(countLabel)
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  private var height: CGFloat {
    max(Self.button, ceil(UIFont.preferredFont(forTextStyle: .body).lineHeight) + 16)
      + Self.padding * 2
  }

  /// Places the bar where the composer rests.
  func install(in host: UIView, area: UILayoutGuide) {
    guard superview !== host else { return }
    host.addSubview(self)
    translatesAutoresizingMaskIntoConstraints = false
    let pull = bottomAnchor.constraint(equalTo: host.bottomAnchor, constant: -Self.padding)
    pull.priority = .defaultHigh
    NSLayoutConstraint.activate([
      leftAnchor.constraint(equalTo: area.leftAnchor, constant: 12),
      rightAnchor.constraint(equalTo: area.rightAnchor, constant: -12),
      bottomAnchor.constraint(lessThanOrEqualTo: host.bottomAnchor, constant: -Self.padding),
      bottomAnchor.constraint(
        lessThanOrEqualTo: host.safeAreaLayoutGuide.bottomAnchor, constant: Self.homeOverlap),
      pull,
      heightAnchor.constraint(equalToConstant: height),
    ])
    if window != nil, !UIAccessibility.isReduceMotionEnabled {
      alpha = 0
      UIView.animate(withDuration: 0.2) { self.alpha = 1 }
    }
  }

  func apply(_ next: SelectionBarState) {
    state = next
    countLabel.text = next.count

    var cancel = UIButton.Configuration.plain()
    cancel.title = next.cancelLabel
    cancel.baseForegroundColor = .label
    cancel.contentInsets = NSDirectionalEdgeInsets(top: 0, leading: 10, bottom: 0, trailing: 10)
    cancelButton.configuration = cancel

    copyButton.accessibilityLabel = next.copyLabel
    copyButton.isEnabled = next.enabled && !next.busy
    saveButton.isEnabled = next.enabled && !next.busy
    style()
    setNeedsLayout()
  }

  /// Colours resolved here and drawn as is: the glass greys template content,
  /// so a live button would read as disabled (as the composer's send).
  private func style() {
    guard let state else { return }
    var copy = UIButton.Configuration.filled()
    copy.cornerStyle = .capsule
    copy.baseBackgroundColor = UIColor.tertiarySystemFill.resolvedColor(with: traitCollection)
    let copyTint = (state.enabled ? UIColor.label : .tertiaryLabel).resolvedColor(
      with: traitCollection)
    copy.image = UIImage(
      systemName: "doc.on.doc",
      withConfiguration: UIImage.SymbolConfiguration(pointSize: 14, weight: .medium)
    )?.withTintColor(copyTint, renderingMode: .alwaysOriginal)
    copyButton.configuration = copy

    var save = UIButton.Configuration.filled()
    save.cornerStyle = .capsule
    let foreground = (state.enabled ? UIColor.systemBackground : .tertiaryLabel).resolvedColor(
      with: traitCollection)
    save.baseBackgroundColor = (state.enabled ? UIColor.label : .tertiarySystemFill)
      .resolvedColor(with: traitCollection)
    save.baseForegroundColor = foreground
    save.attributedTitle = AttributedString(
      state.saveLabel,
      attributes: AttributeContainer([
        .font: UIFont.systemFont(
          ofSize: UIFont.preferredFont(forTextStyle: .subheadline).pointSize, weight: .semibold),
        .foregroundColor: foreground,
      ]))
    save.contentInsets = NSDirectionalEdgeInsets(top: 0, leading: 14, bottom: 0, trailing: 14)
    save.showsActivityIndicator = state.busy
    save.imagePlacement = .trailing
    save.imagePadding = 6
    save.activityIndicatorColorTransformer = UIConfigurationColorTransformer { _ in foreground }
    saveButton.configuration = save
  }

  override func traitCollectionDidChange(_ previous: UITraitCollection?) {
    super.traitCollectionDidChange(previous)
    if previous?.userInterfaceStyle != traitCollection.userInterfaceStyle { style() }
  }

  override func layoutSubviews() {
    super.layoutSubviews()
    backdrop.frame = bounds
    if #unavailable(iOS 26.0) { backdrop.layer.cornerRadius = bounds.height / 2 }
    let pad = Self.padding
    let side = Self.button
    let y = (bounds.height - side) / 2
    let cancelWidth = cancelButton.sizeThatFits(CGSize(width: .greatestFiniteMagnitude, height: side))
      .width
    cancelButton.frame = CGRect(x: pad, y: y, width: cancelWidth, height: side)
    let saveWidth = saveButton.sizeThatFits(CGSize(width: .greatestFiniteMagnitude, height: side))
      .width
    saveButton.frame = CGRect(x: bounds.width - pad - saveWidth, y: y, width: saveWidth, height: side)
    copyButton.frame = CGRect(x: saveButton.frame.minX - pad - side, y: y, width: side, height: side)
    // Centred on the bar, slid aside as far as its text needs to stay clear
    // of Cancel and Copy; truncated only when even the gap is too narrow.
    let left = cancelButton.frame.maxX + pad
    let right = copyButton.frame.minX - pad
    let natural = ceil(countLabel.sizeThatFits(CGSize(width: .greatestFiniteMagnitude, height: side)).width)
    let width = max(0, min(natural, right - left))
    let x = min(max(bounds.width / 2 - width / 2, left), right - width)
    countLabel.frame = CGRect(x: x, y: 0, width: width, height: bounds.height)
  }

  /// An edge swipe carries the page, and the bar with it.
  func setBackProgress(_ progress: CGFloat?) {
    let share = min(1, max(0, progress ?? 0))
    transform = CGAffineTransform(translationX: (superview?.bounds.width ?? 0) * share, y: 0)
  }

  func remove() {
    state = nil
    transform = .identity
    removeFromSuperview()
  }
}
