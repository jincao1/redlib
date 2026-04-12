
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
                let isComment = e.target.classList.contains('md-spoiler-text');
                // parent node is the 'details' node.
                if (!isLink && !isComment)
                    comment.parentNode.removeAttribute('open');
            }
        });
    }
}

function rewriteGiphyLinks() {
    document.querySelectorAll('a[href*="giphy.com/gifs/"]').forEach(link => {
        if (link.querySelector('img')) return;

        const id = link.href.split('/gifs/')[1].split('-').pop();
        const gifUrl = `https://media.giphy.com/media/${id}/giphy.webp`;

        const img = document.createElement('img');
        img.src = gifUrl;
        img.loading = 'lazy';

        link.href = link;
        link.innerHTML = '';
        link.appendChild(img);
    });
}

function addNavBarHandler() {
    var navbar = document.getElementById('feeds');
    document.addEventListener('click', (e) => {
        if (!navbar.contains(e.target)) {
            navbar.removeAttribute('open');
        }
    });
}
document.addEventListener('DOMContentLoaded', function () {
    document.getElementById("search_icon").onclick = toggleSearch;
    addCommentToggleEvent();
    rewriteGiphyLinks();
    addNavBarHandler();
});
