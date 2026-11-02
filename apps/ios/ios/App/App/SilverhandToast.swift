import UIKit

struct ToastNotice {
  enum Tone: String { case success, error, warning, neutral }
  let id, title: String
  let tone: Tone
  /// Replaces the tone's glyph, in a neutral tint.
  let glyph: String?
  let description: String?
  let sticky: Bool
  let action: String?
}

/// A short outcome line in a glass capsule, centered under the bars: it drops
/// in from above, leaves on its own or with a swipe up, and a newer toast takes
/// its place. Drawn natively because the bars sit above the page, where no web
/// toast can reach.
@MainActor
final class ToastView: UIView {
  var onEvent: (([String: Any]) -> Void)?

  private static let height: CGFloat = 44
  private static let lifetime: TimeInterval = 3
  private static let errorLifetime: TimeInterval = 4.5

  private let backdrop: UIVisualEffectView
  private let glyph = UIImageView()
  private let titleLabel = UILabel()
  private let descriptionLabel = UILabel()
  private let actionButton = UIButton(type: .system)
  private var notice: ToastNotice?
  private var dismissal: Timer?
  private var hostTop: NSLayoutConstraint?

  override init(frame: CGRect) {
    if #available(iOS 26.0, *) {
      // Untinted regular glass, as the bars use: it keeps text legible on its
      // own and follows the system's transparency and contrast settings.
      let glass = UIGlassEffect(style: .regular)
      glass.isInteractive = true
      backdrop = UIVisualEffectView(effect: glass)
      backdrop.cornerConfiguration = .capsule()
    } else {
      backdrop = UIVisualEffectView(effect: UIBlurEffect(style: .systemChromeMaterial))
      backdrop.layer.cornerRadius = Self.height / 2
      backdrop.layer.cornerCurve = .continuous
      backdrop.clipsToBounds = true
    }
    super.init(frame: frame)
    isAccessibilityElement = false

    glyph.contentMode = .center
    glyph.setContentHuggingPriority(.required, for: .horizontal)
    glyph.preferredSymbolConfiguration = UIImage.SymbolConfiguration(
      font: .systemFont(ofSize: 15, weight: .semibold))
    titleLabel.font = UIFontMetrics(forTextStyle: .subheadline).scaledFont(
      for: .systemFont(ofSize: 15, weight: .semibold))
    titleLabel.adjustsFontForContentSizeCategory = true
    titleLabel.textColor = .label
    titleLabel.numberOfLines = 2
    descriptionLabel.font = UIFontMetrics(forTextStyle: .footnote).scaledFont(
      for: .systemFont(ofSize: 13))
    descriptionLabel.adjustsFontForContentSizeCategory = true
    descriptionLabel.textColor = .secondaryLabel
    descriptionLabel.numberOfLines = 2
    actionButton.titleLabel?.font = .systemFont(ofSize: 15, weight: .semibold)
    actionButton.setContentHuggingPriority(.required, for: .horizontal)
    actionButton.setContentCompressionResistancePriority(.required, for: .horizontal)
    actionButton.addTarget(self, action: #selector(acted), for: .primaryActionTriggered)

    let lines = UIStackView(arrangedSubviews: [titleLabel, descriptionLabel])
    lines.axis = .vertical
    lines.spacing = 1
    let row = UIStackView(arrangedSubviews: [glyph, lines, actionButton])
    row.spacing = 8
    row.alignment = .center
    addSubview(backdrop)
    backdrop.contentView.addSubview(row)

    for view in [backdrop, row] { view.translatesAutoresizingMaskIntoConstraints = false }
    NSLayoutConstraint.activate([
      backdrop.leadingAnchor.constraint(equalTo: leadingAnchor),
      backdrop.trailingAnchor.constraint(equalTo: trailingAnchor),
      backdrop.topAnchor.constraint(equalTo: topAnchor),
      backdrop.bottomAnchor.constraint(equalTo: bottomAnchor),
      backdrop.heightAnchor.constraint(greaterThanOrEqualToConstant: Self.height),
      row.leadingAnchor.constraint(equalTo: backdrop.contentView.leadingAnchor, constant: 16),
      row.trailingAnchor.constraint(equalTo: backdrop.contentView.trailingAnchor, constant: -18),
      row.topAnchor.constraint(equalTo: backdrop.contentView.topAnchor, constant: 10),
      row.bottomAnchor.constraint(equalTo: backdrop.contentView.bottomAnchor, constant: -10),
    ])

    addGestureRecognizer(UIPanGestureRecognizer(target: self, action: #selector(panned(_:))))
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  /// `top` is where the bars end on the page showing.
  func show(_ next: ToastNotice, in host: UIView, area: UILayoutGuide, top: CGFloat) {
    let replacing = notice?.id == next.id && !isHidden
    notice = next
    fill(next)
    let arriving = superview !== host || isHidden
    if superview !== host {
      removeFromSuperview()
      host.addSubview(self)
      translatesAutoresizingMaskIntoConstraints = false
      let anchor = topAnchor.constraint(equalTo: host.topAnchor, constant: top + 8)
      hostTop = anchor
      NSLayoutConstraint.activate([
        centerXAnchor.constraint(equalTo: area.centerXAnchor),
        leftAnchor.constraint(greaterThanOrEqualTo: area.leftAnchor, constant: 16),
        rightAnchor.constraint(lessThanOrEqualTo: area.rightAnchor, constant: -16),
        widthAnchor.constraint(lessThanOrEqualToConstant: 480),
        anchor,
      ])
    }
    setTop(top)
    host.bringSubviewToFront(self)
    isHidden = false
    if arriving {
      host.layoutIfNeeded()
      alpha = 0
      transform = CGAffineTransform(translationX: 0, y: -24).scaledBy(x: 0.96, y: 0.96)
      UIView.animate(
        withDuration: 0.4, delay: 0, usingSpringWithDamping: 0.82, initialSpringVelocity: 0
      ) {
        self.alpha = 1
        self.transform = .identity
      }
    }
    if !replacing || next.sticky {
      UIAccessibility.post(notification: .announcement, argument: accessibilityText(next))
    }
    scheduleDismissal()
  }

  /// Follows the bars when the page under the toast changes them.
  func setTop(_ top: CGFloat) {
    hostTop?.constant = top + 8
  }

  func dismiss(id: String? = nil) {
    if let id, id != notice?.id { return }
    dismissal?.invalidate()
    guard !isHidden, superview != nil else { return }
    UIView.animate(withDuration: 0.22, delay: 0, options: [.curveEaseIn]) {
      self.alpha = 0
      self.transform = CGAffineTransform(translationX: 0, y: -24)
    } completion: { _ in
      self.isHidden = true
      self.transform = .identity
      self.alpha = 1
      self.notice = nil
    }
  }

  private func fill(_ notice: ToastNotice) {
    let (symbol, tint): (String?, UIColor) =
      switch (notice.glyph, notice.tone) {
      case ("offline", _): ("wifi.slash", .secondaryLabel)
      case (_, .success): ("checkmark.circle.fill", .systemGreen)
      case (_, .error): ("exclamationmark.circle.fill", .systemRed)
      case (_, .warning): ("exclamationmark.triangle.fill", .systemOrange)
      case (_, .neutral): (nil, .secondaryLabel)
      }
    glyph.image = symbol.flatMap { UIImage(systemName: $0) }
    glyph.tintColor = tint
    glyph.isHidden = symbol == nil
    titleLabel.text = notice.title
    descriptionLabel.text = notice.description
    descriptionLabel.isHidden = notice.description == nil
    actionButton.setTitle(notice.action, for: .normal)
    actionButton.isHidden = notice.action == nil
  }

  private func accessibilityText(_ notice: ToastNotice) -> String {
    [notice.title, notice.description].compactMap { $0 }.joined(separator: ", ")
  }

  private func scheduleDismissal() {
    dismissal?.invalidate()
    guard let notice, !notice.sticky else { return }
    let lifetime = notice.tone == .error ? Self.errorLifetime : Self.lifetime
    dismissal = Timer.scheduledTimer(withTimeInterval: lifetime, repeats: false) {
      [weak self] _ in
      Task { @MainActor in self?.dismiss() }
    }
  }

  @objc private func acted() {
    guard let notice else { return }
    onEvent?(["kind": "action", "id": notice.id])
  }

  @objc private func panned(_ pan: UIPanGestureRecognizer) {
    let lift = pan.translation(in: self).y
    switch pan.state {
    case .began:
      dismissal?.invalidate()
    case .changed:
      // Up follows the finger; down only gives a little.
      transform = CGAffineTransform(translationX: 0, y: lift < 0 ? lift : lift / 6)
    case .ended, .cancelled:
      if lift < -16 || pan.velocity(in: self).y < -300 {
        dismiss()
      } else {
        UIView.animate(withDuration: 0.25) { self.transform = .identity }
        scheduleDismissal()
      }
    default:
      break
    }
  }
}
