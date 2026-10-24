import UIKit

struct BannerNotice {
  let id, title: String
  let subtitle, body: String?
  let icon: ComposerPickerRow.Icon?
}

/// A notice over the bars, in their glass: it slides down from the top, leaves
/// on its own or with a swipe up, and reports a tap. The page decides when and
/// what; a newer notice replaces the one showing.
@MainActor
final class BannerView: UIView {
  var onEvent: (([String: Any]) -> Void)?

  private static let radius: CGFloat = 26
  private static let face: CGFloat = 36
  private static let lifetime: TimeInterval = 6

  private let backdrop: UIVisualEffectView
  private let faceView = UIImageView()
  private let titleLabel = UILabel()
  private let bodyLabel = UILabel()
  private var notice: BannerNotice?
  private var dismissal: Timer?
  private var hostTop: NSLayoutConstraint?

  override init(frame: CGRect) {
    if #available(iOS 26.0, *) {
      backdrop = UIVisualEffectView(effect: UIGlassEffect(style: .regular))
      backdrop.cornerConfiguration = .uniformCorners(radius: .fixed(Self.radius))
    } else {
      backdrop = UIVisualEffectView(effect: UIBlurEffect(style: .systemChromeMaterial))
      backdrop.layer.cornerRadius = Self.radius
      backdrop.layer.cornerCurve = .continuous
      backdrop.clipsToBounds = true
    }
    super.init(frame: frame)
    isAccessibilityElement = true
    accessibilityTraits = .button

    faceView.contentMode = .center
    faceView.tintColor = .secondaryLabel
    titleLabel.numberOfLines = 1
    titleLabel.lineBreakMode = .byTruncatingTail
    bodyLabel.numberOfLines = 2
    bodyLabel.font = UIFontMetrics(forTextStyle: .subheadline).scaledFont(
      for: .systemFont(ofSize: 14))
    bodyLabel.adjustsFontForContentSizeCategory = true
    bodyLabel.textColor = .label

    let lines = UIStackView(arrangedSubviews: [titleLabel, bodyLabel])
    lines.axis = .vertical
    lines.spacing = 1
    let row = UIStackView(arrangedSubviews: [faceView, lines])
    row.spacing = 11
    row.alignment = .center
    addSubview(backdrop)
    backdrop.contentView.addSubview(row)

    for view in [backdrop, row, faceView] { view.translatesAutoresizingMaskIntoConstraints = false }
    NSLayoutConstraint.activate([
      backdrop.leadingAnchor.constraint(equalTo: leadingAnchor),
      backdrop.trailingAnchor.constraint(equalTo: trailingAnchor),
      backdrop.topAnchor.constraint(equalTo: topAnchor),
      backdrop.bottomAnchor.constraint(equalTo: bottomAnchor),
      row.leadingAnchor.constraint(equalTo: backdrop.contentView.leadingAnchor, constant: 12),
      row.trailingAnchor.constraint(equalTo: backdrop.contentView.trailingAnchor, constant: -16),
      row.topAnchor.constraint(equalTo: backdrop.contentView.topAnchor, constant: 11),
      row.bottomAnchor.constraint(equalTo: backdrop.contentView.bottomAnchor, constant: -11),
      faceView.widthAnchor.constraint(equalToConstant: Self.face),
      faceView.heightAnchor.constraint(equalToConstant: Self.face),
    ])

    addGestureRecognizer(UITapGestureRecognizer(target: self, action: #selector(tapped)))
    addGestureRecognizer(UIPanGestureRecognizer(target: self, action: #selector(panned(_:))))
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  func show(_ next: BannerNotice, in host: UIView, area: UILayoutGuide) {
    notice = next
    fill(next)
    let arriving = superview !== host || isHidden
    if superview !== host {
      removeFromSuperview()
      host.addSubview(self)
      translatesAutoresizingMaskIntoConstraints = false
      let top = topAnchor.constraint(equalTo: host.safeAreaLayoutGuide.topAnchor, constant: 4)
      hostTop = top
      NSLayoutConstraint.activate([
        leftAnchor.constraint(equalTo: area.leftAnchor, constant: 8),
        rightAnchor.constraint(equalTo: area.rightAnchor, constant: -8),
        top,
      ])
    }
    host.bringSubviewToFront(self)
    isHidden = false
    if arriving {
      host.layoutIfNeeded()
      transform = CGAffineTransform(translationX: 0, y: -(frame.maxY + 8))
      UIView.animate(
        withDuration: 0.45, delay: 0, usingSpringWithDamping: 0.82, initialSpringVelocity: 0
      ) {
        self.transform = .identity
      }
    }
    scheduleDismissal()
  }

  func dismiss() {
    dismissal?.invalidate()
    guard !isHidden, superview != nil else { return }
    UIView.animate(withDuration: 0.25, delay: 0, options: [.curveEaseIn]) {
      self.transform = CGAffineTransform(translationX: 0, y: -(self.frame.maxY + 8))
    } completion: { _ in
      self.isHidden = true
      self.transform = .identity
      self.notice = nil
    }
  }

  private func fill(_ notice: BannerNotice) {
    let size = UIFont.preferredFont(forTextStyle: .subheadline).pointSize
    let title = NSMutableAttributedString(
      string: notice.title,
      attributes: [.font: UIFont.systemFont(ofSize: size, weight: .semibold), .foregroundColor: UIColor.label])
    if let subtitle = notice.subtitle {
      title.append(
        NSAttributedString(
          string: "  \(subtitle)",
          attributes: [.font: UIFont.systemFont(ofSize: size), .foregroundColor: UIColor.secondaryLabel]))
    }
    titleLabel.attributedText = title
    bodyLabel.text = notice.body
    bodyLabel.isHidden = notice.body == nil
    switch notice.icon {
    case .image(let png):
      faceView.image = Data(base64Encoded: png).flatMap { UIImage(data: $0, scale: 3) }.map(
        Self.round)
      faceView.backgroundColor = nil
      faceView.layer.cornerRadius = 0
    case .glyph(let png, let ground):
      // A template the view tints, so it follows the theme as text does.
      faceView.image = BarGlyphs.image(png, size: 20)
      faceView.backgroundColor = ground ? .tertiarySystemFill : nil
      faceView.layer.cornerRadius = Self.face / 2
    case nil:
      faceView.image = nil
    }
    faceView.isHidden = notice.icon == nil
    accessibilityLabel = [notice.title, notice.subtitle, notice.body].compactMap { $0 }.joined(
      separator: ", ")
  }

  /// Drawn round rather than clipped: the glass reshapes the corners of what
  /// it contains.
  private static func round(_ picture: UIImage) -> UIImage {
    let bounds = CGRect(x: 0, y: 0, width: face, height: face)
    return UIGraphicsImageRenderer(bounds: bounds).image { _ in
      UIBezierPath(ovalIn: bounds).addClip()
      picture.draw(in: bounds)
    }
  }

  private func scheduleDismissal() {
    dismissal?.invalidate()
    dismissal = Timer.scheduledTimer(withTimeInterval: Self.lifetime, repeats: false) {
      [weak self] _ in
      Task { @MainActor in self?.dismiss() }
    }
  }

  @objc private func tapped() {
    guard let notice else { return }
    onEvent?(["kind": "open", "id": notice.id])
    dismiss()
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
      if lift < -24 || pan.velocity(in: self).y < -300 {
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
