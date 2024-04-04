
function toggleSearch() {
    let sb = document.getElementById("searchbox");
    if (sb.style.display == "none" || !sb.style.display) {
        sb.style.display = "flex";
    }
    else {
        sb.style.display = "none";
    }
}

window.onload = function () {
    document.getElementById("search_icon").onclick = toggleSearch;
}
