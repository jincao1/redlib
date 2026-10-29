// @license http://www.gnu.org/licenses/agpl-3.0.html AGPL-3.0
// Scrolls the search tab row so the selected tab is in view on narrow screens.
// On load, as the web font changes the tabs' widths.
window.addEventListener("load", () => {
    const tabs = document.querySelector("#listing_options.search_tabs");
    const selected = tabs?.querySelector(".selected");
    if (selected) {
        tabs.scrollLeft += selected.getBoundingClientRect().left - tabs.getBoundingClientRect().left - (tabs.clientWidth - selected.offsetWidth) / 2;
    }
});
// @license-end
