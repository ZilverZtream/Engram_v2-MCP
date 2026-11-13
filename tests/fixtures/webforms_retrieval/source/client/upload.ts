export function validateImageUpload(file: File, maxBytes: number): string | null {
    if (!file.type.startsWith("image/")) return "Only images can be uploaded";
    if (file.size > maxBytes) return "Image exceeds upload size limit";
    return null;
}
