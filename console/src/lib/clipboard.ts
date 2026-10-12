// Copying text, which also works over plain HTTP on the tailnet, where browsers withhold the
// Clipboard API because the page is not a secure context.

export async function copyText(text: string): Promise<boolean> {
	if (window.isSecureContext && navigator.clipboard) {
		try {
			await navigator.clipboard.writeText(text);
			return true;
		} catch {
			// Refused, so the older route below gets a try.
		}
	}
	const area = document.createElement('textarea');
	area.value = text;
	area.setAttribute('readonly', '');
	area.style.position = 'fixed';
	area.style.opacity = '0';
	document.body.appendChild(area);
	const active = document.activeElement as HTMLElement | null;
	area.select();
	let ok = false;
	try {
		ok = document.execCommand('copy');
	} catch {
		ok = false;
	}
	area.remove();
	active?.focus();
	return ok;
}
