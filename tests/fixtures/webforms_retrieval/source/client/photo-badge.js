// Render the remaining required photos beside the upload button.
export function renderPhotoBadge(element, uploaded, required) {
    const remaining = Math.max(0, required - uploaded);
    element.textContent = `${remaining} photos remaining`;
    element.hidden = remaining === 0;
}
