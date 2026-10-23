// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
document.addEventListener("click", async (event) => {
    const link = event.target.closest("a.deeper_replies");
    if (!link || event.ctrlKey || event.metaKey || event.shiftKey || event.button !== 0) return;
    event.preventDefault();
    if (link.dataset.loading) return;
    link.dataset.loading = "1";
    const label = link.textContent;
    link.textContent = "Loading…";

    try {
        // The link opens the parent comment's thread; its replies replace the
        // link, minus those already on the page.
        const response = await fetch(link.href);
        if (!response.ok) throw new Error(response.status);
        const page = new DOMParser().parseFromString(await response.text(), "text/html");
        const parent = page.getElementById(link.closest(".comment").id);
        const replies = [...parent.querySelector(":scope > details > .replies").children];
        const fresh = replies.filter((reply) => (reply.id ? !document.getElementById(reply.id) : reply.getAttribute("href") !== link.getAttribute("href")));
        if (!fresh.length) throw new Error("no new replies");
        link.replaceWith(...fresh);
    } catch {
        link.textContent = label;
        delete link.dataset.loading;
        window.location.href = link.href;
    }
});
// @license-end
