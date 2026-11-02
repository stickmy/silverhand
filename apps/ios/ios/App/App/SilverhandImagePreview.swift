import UIKit

/// An image in a sheet: it fills the width and scrolls when long, pinches to
/// zoom, and shares through the system sheet. The sheet opens at once and shows
/// a spinner until the image arrives. Reports whether a share completed;
/// closing or swiping the sheet away is not one.
@MainActor
final class ImagePreviewController: UIViewController, UIScrollViewDelegate,
  UIAdaptivePresentationControllerDelegate
{
  private var image: UIImage?
  private let shareLabel: String
  private let closeLabel: String
  private let scrollView = UIScrollView()
  private let imageView = UIImageView()
  private let spinner = UIActivityIndicatorView(style: .medium)
  private var finish: ((Bool) -> Void)?
  private var fittedWidth: CGFloat = 0

  /// Presents the sheet empty; `load` supplies the image, decoded off the main
  /// thread so a large photo never stalls the opening animation.
  static func present(
    title: String, shareLabel: String, closeLabel: String, from host: UIViewController,
    load: @escaping () async throws -> Data, finish: @escaping (Result<Bool, Error>) -> Void
  ) {
    let preview = ImagePreviewController(
      title: title, shareLabel: shareLabel, closeLabel: closeLabel)
    preview.finish = { finish(.success($0)) }
    let navigation = UINavigationController(rootViewController: preview)
    navigation.modalPresentationStyle = .pageSheet
    if let sheet = navigation.sheetPresentationController {
      sheet.detents = [.large()]
      sheet.prefersGrabberVisible = false
    }
    navigation.presentationController?.delegate = preview
    host.present(navigation, animated: true)
    Task { @MainActor in
      do {
        let data = try await load()
        let decoded = await Task.detached { UIImage(data: data)?.preparingForDisplay() }.value
        guard let decoded else { throw ImagePreviewError.undecodable }
        preview.show(decoded)
      } catch {
        guard preview.finish != nil else { return }
        preview.finish = nil
        (preview.navigationController ?? preview).presentingViewController?.dismiss(animated: true)
        finish(.failure(error))
      }
    }
  }

  private init(title: String, shareLabel: String, closeLabel: String) {
    self.shareLabel = shareLabel
    self.closeLabel = closeLabel
    super.init(nibName: nil, bundle: nil)
    self.title = title
  }

  required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .systemGroupedBackground

    let close = UIBarButtonItem(
      systemItem: .close, primaryAction: UIAction { [weak self] _ in self?.close(shared: false) })
    close.accessibilityLabel = closeLabel
    navigationItem.leftBarButtonItem = close
    let share = UIBarButtonItem(
      systemItem: .action, primaryAction: UIAction { [weak self] _ in self?.share() })
    share.accessibilityLabel = shareLabel
    share.isEnabled = false
    navigationItem.rightBarButtonItem = share

    scrollView.delegate = self
    scrollView.maximumZoomScale = 4
    scrollView.contentInsetAdjustmentBehavior = .always
    scrollView.showsHorizontalScrollIndicator = false
    view.addSubview(scrollView)
    imageView.isAccessibilityElement = true
    imageView.accessibilityLabel = title
    scrollView.addSubview(imageView)

    let doubleTap = UITapGestureRecognizer(target: self, action: #selector(toggleZoom(_:)))
    doubleTap.numberOfTapsRequired = 2
    scrollView.addGestureRecognizer(doubleTap)

    spinner.translatesAutoresizingMaskIntoConstraints = false
    // A local image arrives within a frame or two; showing the spinner at once
    // would only flash it.
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.2) { [weak self] in
      guard let self, self.image == nil else { return }
      self.spinner.startAnimating()
    }
    view.addSubview(spinner)
    NSLayoutConstraint.activate([
      spinner.centerXAnchor.constraint(equalTo: view.safeAreaLayoutGuide.centerXAnchor),
      spinner.centerYAnchor.constraint(equalTo: view.safeAreaLayoutGuide.centerYAnchor),
    ])
  }

  private func show(_ image: UIImage) {
    self.image = image
    imageView.image = image
    spinner.stopAnimating()
    spinner.removeFromSuperview()
    navigationItem.rightBarButtonItem?.isEnabled = true
    fittedWidth = 0
    view.setNeedsLayout()
  }

  override func viewDidLayoutSubviews() {
    super.viewDidLayoutSubviews()
    scrollView.frame = view.bounds
    let width = scrollView.bounds.width
    guard let image, width > 0, image.size.width > 0, width != fittedWidth else { return }
    fittedWidth = width
    // Zoom 1 is the image at the sheet's width; pinching only enlarges it.
    scrollView.zoomScale = 1
    imageView.frame = CGRect(
      x: 0, y: 0, width: width, height: image.size.height * width / image.size.width)
    scrollView.contentSize = imageView.frame.size
    centre()
  }

  func viewForZooming(in scrollView: UIScrollView) -> UIView? { imageView }

  func scrollViewDidZoom(_ scrollView: UIScrollView) { centre() }

  /// A short image sits in the middle of the sheet rather than at its top.
  private func centre() {
    let visible = scrollView.bounds.inset(by: scrollView.adjustedContentInset)
    let size = scrollView.contentSize
    let dy = max(0, (visible.height - size.height) / 2)
    let dx = max(0, (visible.width - size.width) / 2)
    imageView.center = CGPoint(x: size.width / 2 + dx, y: size.height / 2 + dy)
  }

  @objc private func toggleZoom(_ tap: UITapGestureRecognizer) {
    if scrollView.zoomScale > 1 {
      scrollView.setZoomScale(1, animated: true)
      return
    }
    let point = tap.location(in: imageView)
    let size = CGSize(width: scrollView.bounds.width / 2, height: scrollView.bounds.height / 2)
    scrollView.zoom(
      to: CGRect(
        origin: CGPoint(x: point.x - size.width / 2, y: point.y - size.height / 2), size: size),
      animated: true)
  }

  private func share() {
    guard let image else { return }
    let activity = UIActivityViewController(activityItems: [image], applicationActivities: nil)
    activity.completionWithItemsHandler = { [weak self] _, completed, _, _ in
      if completed { self?.close(shared: true) }
    }
    activity.popoverPresentationController?.barButtonItem = navigationItem.rightBarButtonItem
    present(activity, animated: true)
  }

  private func close(shared: Bool) {
    let finish = self.finish
    self.finish = nil
    // From the presenter, so a share sheet still up leaves with the preview.
    (navigationController ?? self).presentingViewController?.dismiss(animated: true)
    finish?(shared)
  }

  func presentationControllerDidDismiss(_ presentationController: UIPresentationController) {
    let finish = self.finish
    self.finish = nil
    finish?(false)
  }
}

enum ImagePreviewError: LocalizedError {
  case undecodable
  var errorDescription: String? { "Unable to show this image" }
}
