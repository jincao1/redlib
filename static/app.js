
function toggleSearch() {
    let sb = document.getElementById("searchbox");
    if (sb.style.display == "none" || !sb.style.display) {
        sb.style.display = "flex";
    }
    else {
        sb.style.display = "none";
    }
}

function addCommentToggleEvent() {
    let threads = document.getElementsByClassName("thread");
    for (let thread of threads) {
        thread.addEventListener('click', function (e) {
            let comment = e.target.closest('.comment_body');
            if (comment && window.getSelection().toString().length === 0) {
                let tagName = e.target.tagName;
                let isLink = ["A", "IMG"].includes(tagName);
                // parent node is the 'details' node.
                if (!isLink)
                    comment.parentNode.removeAttribute('open');
            }
        });
    }
}

window.onload = function () {
    document.getElementById("search_icon").onclick = toggleSearch;
    addCommentToggleEvent();
}
